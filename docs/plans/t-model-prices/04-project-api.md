# T4 — API types, validation, the rule guard and the project routes

**Commit subject:** `server: add project model price routes`

Adds the `/models` routes of a project (spec 09 § Pricing & GDPR, D-T-7): the effective list, `resolve`, and create/replace/delete of project rows; the write path shared with T5 (validation D-T-4, the rule guard D-T-8, the reload D-T-9); the OpenAPI operations; and the fixtures. It also points the budget error at the new page (D-T-11).

## Read first

- README § Design → Writes, D-T-4, D-T-6, D-T-7, D-T-8, D-T-11, § Conventions.
- T1–T3: `PriceTable::layered`, `exact_pattern`; `meta::model_prices`; `crate::prices` (`Prices`, `build_org`, `OrgTables`).
- `crates/server/src/routes/score_definitions.rs` (the model for routes, handlers, `page_named`, `field_error`), `crates/server/src/routes/connections.rs` (audit lines with `target: "audit"`), `crates/server/src/routes/evaluators.rs` (`broken_rules`, `Outcome::BreaksRules` → `ApiError::ConflictWith`).
- `crates/server/src/eval/rule_check.rs` (`Subject::from_row`, `check_rule`'s budget message), `crates/server/src/eval/budget.rs` (`model_price`).
- `crates/server/src/http/access.rs` (`RouteDef`, `Access`, `ProjectAccess`, `routes()`), `crates/server/src/http/error.rs` (`ApiError::{BadRequest, NotFound, Conflict, ConflictWith}`, `field_error`).
- `crates/server/src/api/score_definitions.rs` (types, `Limit`, `crate::api::keys::optional_time`), `crates/server/src/api/common.rs` (`Page`, `FieldError`, `Limit`).
- `crates/server/src/api/openapi.rs` (`collect_schemas`, the `.tags(…)` list, `paths()`, `score_definition_paths`, helpers `operation`, `project_param`, `path_uuid`, `with_query`, `json_named`, `json_of`, `json_body`, `responses`, the error-code constants), `crates/server/tests/openapi.rs` (`OPERATIONS`, `TAGS`, the `expected_errors` chain, the `pages` lists).
- `crates/server/src/api_examples.rs` (the static `example(…)` entries, `example_created`), `crates/server/tests/api_examples.rs` (`roundtrip::<…>(…)`, the request list near `("post_online_rule", "requests/online-rule-create.json")`), `fixtures/api/README.md`.

## Files

| File | Change |
|---|---|
| `crates/server/src/api/model_prices.rs` | new: types |
| `crates/server/src/api/mod.rs` | `pub mod model_prices;` (alphabetical: after `metrics`) |
| `crates/server/src/prices.rs` | `validate_input`, `PriceChange`, `WriteOutcome`, `write`, `record_json`, `builtin_json` |
| `crates/server/src/routes/model_prices.rs` | new: project routes |
| `crates/server/src/routes/mod.rs` | `pub(crate) mod model_prices;` (alphabetical) |
| `crates/server/src/http/access.rs` | `routes.extend(crate::routes::model_prices::project_routes());` in `routes()`, after the score definition routes |
| `crates/server/src/eval/rule_check.rs` | the budget message |
| `crates/server/src/api/openapi.rs` | schemas, tag `model_prices`, `model_price_paths` |
| `crates/server/tests/openapi.rs` | operations, tag, errors, pages |
| `crates/server/openapi.json`, `web/src/api/schema.gen.ts` | regenerated (`just openapi`) |
| `fixtures/api/static/model-prices.json`, `model-price.json`, `model-price-resolution.json` | new |
| `fixtures/api/requests/model-price-create.json` | new |
| `fixtures/api/README.md` | two fixed ids |
| `crates/server/src/api_examples.rs`, `crates/server/tests/api_examples.rs` | examples and round-trips |
| `fixtures/api/manifest.json` | regenerated (`just api-examples`) |
| `CHANGELOG.md` | one `Added` line |

## 1. Types (`api/model_prices.rs`)

Module doc: `//! Model price request and response types (spec 09 § Pricing & GDPR, spec 04 § Pricing, D-T-7).`

```rust
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::common::Limit;

/// Who owns a price row. Project beats org, which beats built-in (spec 02 rule 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelPriceScope {
    /// The embedded LiteLLM snapshot. Read-only.
    Builtin,
    /// A row of the organization; applies to all its projects.
    Org,
    /// A row of one project.
    Project,
}

/// One long-context band. A null price uses the row's base price.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelPriceTier {
    /// The band applies when the span's input tokens (cache included) are at least this count.
    pub min_input_tokens: i64,
    /// USD per 1M uncached input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_input: Option<f64>,
    /// USD per 1M output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_output: Option<f64>,
    /// USD per 1M cache-read tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_cache_read: Option<f64>,
    /// USD per 1M cache-write tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_cache_write: Option<f64>,
    /// USD per 1M reasoning tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_reasoning: Option<f64>,
}

/// A price row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ModelPrice {
    /// Row id. Absent on built-in rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    /// Who owns the row.
    pub scope: ModelPriceScope,
    /// Display name.
    pub model_name: String,
    /// Regular expression matched against the span's model, anchored and case-insensitive.
    pub match_pattern: String,
    /// When set, the row prices spans of this provider (or with none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The row prices spans that started at or after this time. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub valid_from: i64,
    /// USD per 1M uncached input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_input: Option<f64>,
    /// USD per 1M output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_output: Option<f64>,
    /// USD per 1M cache-read tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_cache_read: Option<f64>,
    /// USD per 1M cache-write tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_cache_write: Option<f64>,
    /// USD per 1M reasoning tokens. Absent: the output price.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_reasoning: Option<f64>,
    /// Long-context bands, by increasing `min_input_tokens`. Empty when none.
    pub tiers: Vec<ModelPriceTier>,
    /// Creation time. Absent on built-in rows. JSON is RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::api::keys::optional_time")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub created_at: Option<i64>,
    /// Last change. Absent on built-in rows. JSON is RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::api::keys::optional_time")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub updated_at: Option<i64>,
}

/// Body of `POST /models` and `PATCH /models/{id}`. A PATCH replaces the whole row: an absent
/// optional field becomes null (D-T-7).
#[derive(Debug, Clone, PartialEq, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelPriceInput {
    /// Display name, 1–200 characters after trimming.
    pub model_name: String,
    /// Regular expression, 1–500 bytes. Absent or blank: exactly `model_name`.
    #[serde(default)]
    pub match_pattern: Option<String>,
    /// Provider, such as `openai` or `gcp.vertex_ai`. Absent or blank: any provider.
    #[serde(default)]
    pub provider: Option<String>,
    /// Absent: the row applies at any time. JSON is RFC 3339.
    #[serde(default, with = "crate::api::keys::optional_time")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub valid_from: Option<i64>,
    /// USD per 1M uncached input tokens, 0–1 000 000.
    #[serde(default)]
    pub price_input: Option<f64>,
    /// USD per 1M output tokens, 0–1 000 000.
    #[serde(default)]
    pub price_output: Option<f64>,
    /// USD per 1M cache-read tokens.
    #[serde(default)]
    pub price_cache_read: Option<f64>,
    /// USD per 1M cache-write tokens.
    #[serde(default)]
    pub price_cache_write: Option<f64>,
    /// USD per 1M reasoning tokens. Absent: the output price.
    #[serde(default)]
    pub price_reasoning: Option<f64>,
    /// Up to 8 bands, by strictly increasing `min_input_tokens`.
    #[serde(default)]
    pub tiers: Vec<ModelPriceTier>,
}

/// Query parameters of `GET /api/v1/projects/{project}/models`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct ModelPriceListParams {
    /// Only rows of this scope.
    #[serde(default)]
    pub scope: Option<ModelPriceScope>,
    /// Case-insensitive substring of `model_name`.
    #[serde(default)]
    pub q: Option<String>,
    /// Maximum number of items to return (1–1000, default 50).
    #[serde(default)]
    #[param(minimum = 1, maximum = 1000, default = 50)]
    pub limit: Limit,
    /// Opaque cursor from a previous page.
    #[serde(default)]
    pub cursor: Option<String>,
}

/// Query parameters of `GET /api/v1/projects/{project}/models/resolve`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct ModelPriceResolveParams {
    /// The model name a span would carry.
    pub model: String,
    /// The provider a span would carry. Absent: none.
    #[serde(default)]
    pub provider: Option<String>,
}

/// The row that would price a span now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ModelPriceResolution {
    /// The winning row. Absent when no row prices the model (spec 09: optional fields are
    /// absent, never null).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<ModelPrice>,
}
```

An enum field in a params struct works as `EvalJobListParams.kind: Option<EvalJobKind>` does in `crates/server/src/api/eval_jobs.rs` (the enum derives `ToSchema`, the struct `IntoParams`).

## 2. The write path (`prices.rs`, added below T3's code)

Imports to add: `uuid::Uuid`, `cetra_otel::{PriceScope, PriceTier, exact_pattern}`, `cetra_eval::Adapter`, `crate::api::common::FieldError`, `crate::api::model_prices::{ModelPrice, ModelPriceInput, ModelPriceScope, ModelPriceTier}`, `crate::eval::budget::model_price`, `crate::eval::rule_check::Subject`, `crate::http::AppState`, `crate::meta::{connections, evaluators}`.

### Constants

```rust
const MAX_NAME_CHARS: usize = 200;
const MAX_PATTERN_BYTES: usize = 500;
const MAX_PROVIDER_CHARS: usize = 64;
const MAX_TIERS: usize = 8;
const MAX_PRICE: f64 = 1_000_000.0;
```

### `validate_input`

```rust
/// Checks a body against D-T-4 and builds the row of `scope` (`Org` or `Project`). Every
/// failing field is reported.
pub(crate) fn validate_input(input: &ModelPriceInput, scope: PriceScope) -> Result<PriceRow, Vec<FieldError>>;
```

Checks, in this order, each pushing a `FieldError { field, message, position: None }` (field paths exactly as written):

1. `model_name`, trimmed: empty → `"is required"`; more than 200 `chars()` → `"at most 200 characters"`.
2. `match_pattern`: `None`, or blank after trimming → `exact_pattern(<trimmed name>)` (only when the name is valid). Otherwise the trimmed pattern: more than 500 bytes → `"at most 500 bytes"`; else `PriceTable::new(vec![<a row with this pattern>])`: `Err(PriceError::InvalidPattern { .. })` → `"is not a valid regular expression"`, `Err(PriceError::TooLarge)` → `"is too large"`.
3. `provider`: `None` or blank → `None`. Otherwise trimmed: more than 64 chars, or the first char not ASCII lowercase or digit, or any char outside ASCII lowercase, digits, `.`, `_`, `-` → `"lowercase letters, digits, '.', '_' and '-', at most 64"`.
4. `valid_from`: `None` → 0; negative → `"must not be before 1970-01-01"`.
5. Each of `price_input`, `price_output`, `price_cache_read`, `price_cache_write`, `price_reasoning` that is `Some(v)` with `!v.is_finite() || v < 0.0 || v > MAX_PRICE` → field `price_<part>`, `"must be between 0 and 1000000"`.
6. `price_input` and `price_output` both `None` → field `price_input`, `"set an input or an output price"`.
7. `tiers`: more than 8 → field `tiers`, `"at most 8 tiers"` (skip the per-tier checks). For tier `i` (0-based): `min_input_tokens < 1` → `tiers[i].min_input_tokens`, `"must be at least 1"`; `i > 0` and not greater than tier `i-1`'s → `tiers[i].min_input_tokens`, `"must be greater than the previous tier's"`; each price as rule 5 with field `tiers[i].price_<part>`; all five prices `None` → `tiers[i]`, `"set at least one price"`.

On success: `PriceRow { scope, model_name: trimmed, match_pattern, provider, valid_from, price_*: as given, tiers: if empty { None } else { Some(<each ModelPriceTier as PriceTier, field by field>) } }`.

### `PriceChange`, `WriteOutcome`, `write`

```rust
/// One price write.
pub(crate) enum PriceChange {
    /// A new row with this id.
    Insert { id: Uuid, row: PriceRow },
    /// Replace the row `id`.
    Update { id: Uuid, row: PriceRow },
    /// Delete the row `id`.
    Delete { id: Uuid },
}

/// What a write did.
pub(crate) enum WriteOutcome {
    /// Inserted or updated: the stored row. Deleted: `None`.
    Done(Option<PriceRecord>),
    /// No row `id` in that scope.
    NotFound,
    /// The scope's table would not compile (D-T-4).
    Invalid(Vec<FieldError>),
    /// Budgeted rules would lose their price (D-T-8).
    BreaksRules(Vec<FieldError>),
}

/// Applies `change` to the rows of `org` in the scope `project` (`None`: the org's rows), in
/// one write transaction, then reloads the org's tables (README § Writes).
///
/// # Errors
///
/// [`MetaError`] from the write: `Conflict` for the caps and the unique key.
pub(crate) async fn write(
    state: &AppState,
    org: OrgId,
    project: Option<ProjectId>,
    change: PriceChange,
) -> Result<WriteOutcome, MetaError>;
```

Remove the `#![cfg_attr(not(test), allow(dead_code))]` line T3 put in `prices.rs`: every function now has a non-test caller. (The one in `meta/model_prices.rs` stays until T5: `list_org_rows` is used by T5's org list only.)

Body of `write`:

1. `let prices = state.ingest.prices(); let _guard = prices.lock_writes().await; let builtin = prices.builtin(); let now = state.clock.now_ns();`
2. `let outcome = state.meta.write(move |conn| Box::pin(async move { … })).await?;` (clone `builtin` into the closure; the closure owns `change`). Inside:
   1. `let records = model_prices::list_org_all_tx(conn, org).await?;`
   2. For `Update` and `Delete`: `if model_prices::get_tx(conn, org, project, id).await?.is_none() { return Ok(WriteOutcome::NotFound); }`
   3. `let mut candidate = records.clone();` then: `Insert` → push `PriceRecord { id, org_id: org, project_id: project, row: row.clone(), created_at: now, updated_at: now }`; `Update` → set `row` of the record with that id; `Delete` → `retain` the others.
   4. `let after = match build_org(&builtin, &candidate) { Ok(t) => t, Err(PriceError::TooLarge) => return Ok(WriteOutcome::Invalid(vec![FieldError { field: "match_pattern".into(), message: "the patterns of this scope are too large together".into(), position: None }])), Err(PriceError::InvalidPattern { .. }) => return Ok(WriteOutcome::Invalid(vec![FieldError { field: "match_pattern".into(), message: "is not a valid regular expression".into(), position: None }])) };`
   5. `let before = build_org(&builtin, &records).ok();` (stored rows that no longer compile count as built-in only, as the registry does).
   6. `let broken = broken_rules(conn, org, project, &builtin, before.as_ref(), &after, now).await?; if !broken.is_empty() { return Ok(WriteOutcome::BreaksRules(broken)); }`
   7. Apply: `Insert` → `Some(model_prices::insert_tx(conn, id, org, project, &row, now).await?)`; `Update` → `match model_prices::update_tx(…).await? { Some(r) => Some(r), None => return Ok(WriteOutcome::NotFound) }`; `Delete` → `model_prices::delete_tx(…).await?; None`. `Ok(WriteOutcome::Done(that))`.
3. If `outcome` is `Done(_)`: `if let Err(error) = prices.reload_org(state.meta.reader(), org).await { tracing::error!(org_id = %org, error = %error, "prices were written but not reloaded"); }`. The write is committed, so the answer stays a success; the next write or a restart reloads. (`MetaError`'s `Display` carries no row values; check, and log only `org_id` if it might.)
4. `Ok(outcome)`; the guard drops last.

Private helpers:

```rust
fn table_of<'a>(tables: Option<&'a OrgTables>, builtin: &'a Arc<PriceTable>, project: ProjectId) -> &'a PriceTable {
    match tables {
        Some(t) => t.projects.get(&project).or(t.org.as_ref()).unwrap_or(builtin),
        None => builtin,
    }
}

/// D-T-8: the budgeted judge rules in scope that have a price before the change and none after.
async fn broken_rules(
    conn: &mut SqliteConnection,
    org: OrgId,
    project: Option<ProjectId>,
    builtin: &Arc<PriceTable>,
    before: Option<&OrgTables>,
    after: &OrgTables,
    now: i64,
) -> Result<Vec<FieldError>, MetaError>;
```

`broken_rules`: for each `rule` of `model_prices::budgeted_rules_tx(conn, org, project)`: load `evaluators::get_evaluator_tx(conn, rule.project_id, rule.evaluator_id)` (`None` → skip); `Subject::from_row(&row)` (error → `MetaError::Corrupt { table: "evaluators", column: "config" }`); not `Subject::Judge(judge)` → skip; `connections::get_connection_tx(conn, org, judge.connection_id)` (`None` → skip: the rule is already broken by its connection, not by prices); `Adapter::from_name(&connection.adapter)` (error → `Corrupt { table: "llm_connections", column: "adapter" }`); `priced_before = model_price(table_of(before, builtin, rule.project_id), &judge.model, adapter, now).is_some()`; `priced_after = model_price(table_of(Some(after), builtin, rule.project_id), …).is_some()`; `priced_before && !priced_after` → push `FieldError { field: format!("rules.{}", rule.id), message: format!("{}: the judge model would have no price", rule.name), position: None }`. A rule already unpriced before the change is not reported, so a write never fails on a rule it did not break.

### JSON conversions

```rust
/// A stored row as JSON.
pub(crate) fn record_json(record: &PriceRecord) -> ModelPrice;
/// A built-in row as JSON (no id, no times).
pub(crate) fn builtin_json(row: &PriceRow) -> ModelPrice;
```

Both map `PriceRow` field by field; `scope` from `row.scope` (`Builtin` → `ModelPriceScope::Builtin`, etc.); `tiers` → `Vec<ModelPriceTier>` (`None` → empty). `record_json` sets `id`, `created_at`, `updated_at`; `builtin_json` leaves them `None`.

### Tests for this section (`prices.rs` tests)

1. `validate_accepts_a_minimal_row`: `{model_name: " llama3.1:8b ", price_input: 0, price_output: 0}` → name trimmed, `match_pattern == exact_pattern("llama3.1:8b")`, provider `None`, `valid_from` 0, tiers `None`.
2. `validate_reports_every_field`: a body with an empty name, a 501-byte pattern, provider `"OpenAI"`, `valid_from` −1, `price_input` −1, `price_output` NaN is impossible in JSON, so use `price_output: 1e7`, and tiers `[{min 0}, {min 0, price_input: 1}]` → errors for `model_name`, `match_pattern`, `provider`, `valid_from`, `price_input`, `price_output`, `tiers[0].min_input_tokens`, `tiers[0]`, `tiers[1].min_input_tokens` (assert the exact list of fields, in this order).
3. `validate_refuses_a_pattern_that_only_works_wrapped`: `match_pattern: "x)|(.*"` → `match_pattern` error (the T1 guard against widening the anchor).
4. `validate_needs_input_or_output`: only `price_cache_read` → field `price_input`.
5. `validate_allows_nine_tiers_no_more`: 8 increasing tiers pass; 9 → `tiers`.
6. `validate_provider_rules`: `"gcp.vertex_ai"`, `"x_ai"`, `"azure.ai.openai"` pass; `".x"`, `"a b"`, 65 chars fail; `"  "` is `None`.
7. `record_and_builtin_json_round_trip_the_prices`.

## 3. The budget message (`eval/rule_check.rs`)

Change `"the judge model has no price, so the rule cannot have a budget"` to `"the judge model has no price, so the rule cannot have a budget; add one under Settings → Model prices"` (D-T-11). Update every test that asserts the old text:

```bash
grep -rn "so the rule cannot have a budget" crates/server/src web/src
```

(The web does not show this text itself; if a web test fixture contains it, update it too.)

## 4. Routes (`routes/model_prices.rs`)

Module doc: `//! Model prices (spec 09 § Pricing & GDPR, D-T-6, D-T-7). Project routes here; org routes in T5.`

### Route table

```rust
pub(crate) fn project_routes() -> Vec<RouteDef> {
    vec![
        // GET    /api/v1/projects/{project}/models           View              list_models
        // GET    /api/v1/projects/{project}/models/resolve   View              resolve_model_price
        // POST   /api/v1/projects/{project}/models           ManageSettings    create_model_price
        // PATCH  /api/v1/projects/{project}/models/{id}      ManageSettings    update_model_price
        // DELETE /api/v1/projects/{project}/models/{id}      ManageSettings    delete_model_price
    ]
}
```

Write each as a `RouteDef { method, path, access: Access::Project { perm, session_only: false }, documented: true, handler }` exactly as `score_definitions.rs` does. Axum matches the static segment `resolve` before `{id}`; the `GET …/models/{id}` route does not exist, so there is no clash.

### Handlers

```rust
#[derive(Debug, Deserialize)]
struct PricePath { id: Uuid }

async fn list_models(State(state): State<AppState>, Extension(access): Extension<ProjectAccess>,
    error::Query(params): error::Query<ModelPriceListParams>) -> Result<error::Json<Page<ModelPrice>>, ApiError>;
async fn resolve_model_price(State(state): State<AppState>, Extension(access): Extension<ProjectAccess>,
    error::Query(params): error::Query<ModelPriceResolveParams>) -> Result<error::Json<ModelPriceResolution>, ApiError>;
async fn create_model_price(State(state): State<AppState>, Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>, error::Json(body): error::Json<ModelPriceInput>)
    -> Result<(StatusCode, error::Json<ModelPrice>), ApiError>;
async fn update_model_price(State(state): State<AppState>, Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>, error::Path(PricePath { id }): error::Path<PricePath>,
    error::Json(body): error::Json<ModelPriceInput>) -> Result<error::Json<ModelPrice>, ApiError>;
async fn delete_model_price(State(state): State<AppState>, Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>, error::Path(PricePath { id }): error::Path<PricePath>)
    -> Result<StatusCode, ApiError>;
```

`error::Query` is the extractor `list_score_definitions` uses. In every handler `let org = access.project.org_id; let project = access.project.id;`.

**`list_models`**:
1. `q` trimmed and lowercased; blank → none. More than 200 chars → `field_error("q", "at most 200 characters")`.
2. `let builtin = state.ingest.prices().builtin();` (the list needs no compiled project table).
3. Custom rows: `model_prices::list_org_all(state.meta.reader(), org)` keeping org rows and this project's rows only (drop other projects' rows). Reading `meta.db` keeps ids and times; the table only has `PriceRow`s.
4. Build `Vec<(String, String, ModelPrice)>`: for each kept record, `(format!("{rank}:{}", record.row.model_name.to_lowercase()), record.id.to_string(), record_json(&record))` with `rank` `0` for project rows, `1` for org rows; for each `(index, row)` of `builtin.rows()`, `(format!("2:{}", row.model_name.to_lowercase()), format!("builtin:{index:06}"), builtin_json(row))`.
5. Filter by `params.scope` and by `q` (substring of the lowercased name).
6. Sort by `(name, id)`, then `page_named(rows, params.cursor.as_deref(), params.limit.get())` (from `super::orgs`). Return `error::Json(page)`.

The `rank:` prefix keeps the D-T-7 order, and the id tie-break keeps the cursor stable.

**`resolve_model_price`**: `model` trimmed; empty → `field_error("model", "is required")`; more than 500 bytes → `field_error("model", "at most 500 bytes")`. `provider` trimmed, blank → `None`. `let table = state.ingest.prices().for_project(org, project); let now = state.clock.now_ns();` `let Some(row) = table.price_for(&model, provider.as_deref(), now) else { return Ok(error::Json(ModelPriceResolution { price: None })) };` (serialized as `{}`). A built-in row (`row.scope == PriceScope::Builtin`) → `builtin_json(row)`. A custom row needs its id: look it up among `model_prices::list_org_all(reader, org)` by the scope and the full `PriceRow` equality (`record.row == *row` and, for a project row, `record.project_id == Some(project)`); found → `record_json`; not found (a reload raced) → `builtin_json(row)` with the right `scope` (set `scope` from `row.scope`). Answer `{price: Some(…)}`.

**`create_model_price`**, in this order:
1. `let user = principal::session_user(&principal)?;` (it accepts a session and a PAT, and refuses an API key, which `ManageSettings` already keeps out).
2. `let row = prices::validate_input(&body, PriceScope::Project).map_err(|errors| ApiError::BadRequest { detail: None, errors })?;`
3. `let id = Uuid::now_v7();` `prices::write(&state, org, Some(project), PriceChange::Insert { id, row })`.
4. Map the outcome: `Done(Some(record))` → audit `tracing::info!(target: "audit", price_id = %id, org_id = %org, project_id = %project, user_id = %user.id, "model price created")`, then `Ok((StatusCode::CREATED, error::Json(record_json(&record))))`; `Invalid(errors)` → `ApiError::BadRequest { detail: None, errors }`; `BreaksRules(errors)` → `ApiError::ConflictWith { detail: "the change would leave budgeted online rules without a price", errors }`; `NotFound` and `Done(None)` cannot happen on insert → `ApiError::NotFound`. A `MetaError::Conflict` becomes `409` through `From<MetaError>` already.

**`update_model_price`**: the same with `PriceChange::Update { id, row }`; `NotFound` → `ApiError::NotFound`; `Done(Some(record))` → audit `"model price updated"` and `200` with `record_json`.

**`delete_model_price`**: `prices::write(…, PriceChange::Delete { id })`; `NotFound` → `404`; `BreaksRules` → `409` as above; `Done(None)` → audit `"model price deleted"`, `204`.

### Tests (`routes/model_prices.rs` `mod tests`, with `TestApp`)

Use the helpers the score definition route tests use (`app.seed()`, requests as the seeded admin, a member, a viewer, an API key with `read`). The seeded project's builtin table is the real snapshot.

1. `create_list_update_delete_round_trip`: POST `{model_name: "llama3.1:8b", price_input: 0, price_output: 0}` → `201`, `scope: "project"`, `match_pattern: "llama3\\.1:8b"`, `tiers: []`; GET `?scope=project` lists it; PATCH with `{model_name: "llama3.1:8b", price_input: 0.1, price_output: 0.2, provider: "openai"}` → `200`, provider set; PATCH again without `provider` → provider absent (replace semantics); DELETE → `204`; GET `?scope=project` → empty.
2. `the_list_orders_project_org_builtin_and_pages`: insert an org row through `model_prices::insert_tx` + `reload_org`, and a project row through the API; GET `?limit=2` → the project row then the org row, `next_cursor` set; the next page starts with built-in rows; `?scope=builtin&q=gpt-4o&limit=1000` → only built-in rows whose name contains `gpt-4o`; `?q=` of 201 chars → `400`.
3. `other_projects_rows_are_not_listed_or_used`: a row created in a second project of the same org is not in the seeded project's list, and `resolve` in the seeded project does not return it.
4. `resolve_names_the_winning_row`: `?model=gpt-4o&provider=openai` → a built-in row; after a project row `gpt-4o` at 1.0 → that project row with its id; `?model=no-such-model` → `{}` (no `price` key); `?model=` → `400`.
5. `a_new_price_applies_to_spans_ingested_after_it`: ingest one OTLP span for model `llama3.1:8b` (build the request as the T3 ingest tests or `routes/otlp.rs` tests do) → `cost_source` `none`; POST the price → ingest another span → `computed`. Read the first span again: still `none` (D-T-5).
6. `validation_errors_are_400`: an empty body name, a bad pattern, no prices → `400` with those fields.
7. `a_duplicate_is_409` and `the_501st_project_row_is_409` (insert 500 through the store, then POST).
8. `deleting_the_price_of_a_budgeted_rule_is_409`: create a connection and a judge evaluator on `llama3.1:8b` (the online rule tests' helpers), POST the price, create a budgeted rule (`201`), then DELETE the price → `409` with one error `rules.<rule id>`; PATCH it to `price_output` only and `price_input` absent → `409` too (`model_price` needs both); PATCH it to other prices → `200`.
9. `a_write_does_not_fail_on_a_rule_it_did_not_break`: a budgeted rule whose model has no price at all (insert the rule with SQL, since the route refuses it) does not block creating an unrelated price.
10. `permissions`: a project member gets `403` on POST/PATCH/DELETE; a viewer can GET; a `read` API key can GET and `resolve`; any API key gets `403` on POST.
11. `writes_are_audited`: capture logs the way `routes/connections.rs`'s audit test does and check the three messages carry ids and no model name.

## 5. OpenAPI

In `api/openapi.rs`:
- Register the schemas in `collect_schemas`: `ModelPriceScope`, `ModelPriceTier`, `ModelPrice`, `ModelPriceInput`, `ModelPriceResolution`, and the named page `ModelPricePage` (as `ScoreDefinitionPage` is registered).
- Add the tag `model_prices` ("Custom and built-in model prices") to `.tags(…)`, in the list's order.
- Add `fn model_price_paths(paths: PathsBuilder) -> PathsBuilder`, modelled on `score_definition_paths`, and call it from `paths()` next to it:

| Operation id | Method, path | Params | Body | Success | Errors |
|---|---|---|---|---|---|
| `list_models` | GET `/api/v1/projects/{project}/models` | `with_query::<ModelPriceListParams>(vec![project_param()])` | — | `200` `json_named("ModelPricePage", "One page of the project's effective price table.")` | `QUERY_ERRORS` |
| `resolve_model_price` | GET `/api/v1/projects/{project}/models/resolve` | `with_query::<ModelPriceResolveParams>(vec![project_param()])` | — | `200` `json_of::<ModelPriceResolution>(…)` | `QUERY_ERRORS` |
| `post_model_price` | POST `/api/v1/projects/{project}/models` | `project_param()` | `json_body("ModelPriceInput")` | `201` `json_of::<ModelPrice>(…)` | `&[400, 401, 403, 404, 409, 429]` |
| `patch_model_price` | PATCH `/api/v1/projects/{project}/models/{id}` | `project_param()`, `path_uuid("id", "Price row id.")` | `json_body("ModelPriceInput")` | `200` `json_of::<ModelPrice>(…)` | `&[400, 401, 403, 404, 409, 429]` |
| `delete_model_price` | DELETE same path | same | — | `204` | `&[400, 401, 403, 404, 409, 429]` |

  Descriptions: reads "Any project member can read; a `read` API key too."; writes "Project admins. API keys cannot call it. A PATCH replaces the whole row. A write that would leave a budgeted online rule without a price is 409."

In `tests/openapi.rs`:
- `OPERATIONS`: the five `(method, path, operation id)` tuples, next to the score definition ones.
- `TAGS`: `"model_prices"`.
- `QUERY_OR_BODY`: add `"list_models"` and `"resolve_model_price"` (they expect 400, 401, 403, 404, 429; without this the default branch expects no 400 and the test fails).
- The `expected_errors` chain: add `|| *id == "post_model_price" || *id == "patch_model_price" || *id == "delete_model_price"` to the branch that holds `post_score_definition` (400, 401, 403, 404, 409, 429). Not the `delete_score_definition` branch: a price delete can be `409` (D-T-8).
- The `pages` array of `(page, item)` pairs (it holds `("ScorePage", "Score")`): add `("ModelPricePage", "ModelPrice")`.
- The second `pages` list, in the nullability check (it holds `"TracePage"`, `"SpanPage"`, …; it must equal the set of pages with a required-nullable `next_cursor`): add `"ModelPricePage"`.

Run `just openapi`.

## 6. Fixtures

Fixed ids (add to `fixtures/api/README.md` § Fixed identifiers, after the backfills row):

| Thing | Value |
|---|---|
| Model prices | `01900000-0000-7000-8000-000000000c00`, an org row `Llama 3.1 8B (local)` for `llama3\.1:8b`, any provider, 0 USD; `…0c01`, a project row `gpt-4o (negotiated)` for `gpt\-4o`, provider `openai`, from `2026-01-01T00:00:00.000000000Z`, input 2.0, output 8.0, cache read 1.0. `model-prices.json`, `model-price.json`, `model-price-resolution.json`, `org-model-prices.json` and `requests/model-price-create.json` use them |

Files (times `2026-01-02T10:00:00.000000000Z` for created/updated). `tests/api_examples.rs`'s `roundtrip` compares the file text with the canonical JSON of the parsed value (pretty, struct field order, floats as Rust prints them: `0.0`, `2.0`), so do not hand-format: write the values below, run `cargo test -p cetra-server --test api_examples static_files_match_their_types`, and copy the expected text from each failure into the file until it passes. Content:
- `static/model-prices.json`: a `ModelPricePage` with the project row `…0c01`, the org row `…0c00`, then two built-in rows copied from `crates/otel/prices/builtin.json` (`gpt-4o` provider `openai`, and `gpt-4o-mini` provider `openai`; copy their prices exactly, `tiers: []`, no id, no times), `next_cursor: null` (no static fixture carries a cursor; the generator checks only that the file round-trips).
- `static/model-price.json`: the project row `…0c01` alone.
- `static/model-price-resolution.json`: `{"price": <the project row …0c01>}` (the found case).
- `requests/model-price-create.json`: `{"model_name": "gpt-4o (negotiated)", "match_pattern": "gpt\\-4o", "provider": "openai", "valid_from": "2026-01-01T00:00:00.000000000Z", "price_input": 2.0, "price_output": 8.0, "price_cache_read": 1.0}`.

In `api_examples.rs`: `example("list_models", BTreeMap::new(), BTreeMap::new(), "static/model-prices.json", "application/json")`; `example("resolve_model_price", BTreeMap::new(), params(&[("model", "gpt-4o"), ("provider", "openai")]), "static/model-price-resolution.json", "application/json")` (the arguments are path params, then query); `example_created("post_model_price", BTreeMap::new(), "static/model-price.json")`. In `tests/api_examples.rs`: `roundtrip::<ModelPricePage-or-Page<ModelPrice>>("static/model-prices.json")` as the score definition page is round-tripped, `roundtrip::<ModelPrice>("static/model-price.json")`, `roundtrip::<ModelPriceResolution>(…)`, `roundtrip::<ModelPriceInput>("requests/model-price-create.json")` (it has `Deserialize` only; use the helper that other request files use), and `("post_model_price", "requests/model-price-create.json")` in the request list.

Run `just api-examples` and commit `fixtures/api/manifest.json`.

## 7. CHANGELOG

Under `## [Unreleased]` → `### Added`: `- Custom model prices per project: \`/api/v1/projects/{project}/models\` lists the effective price table, resolves which price applies, and adds, replaces and deletes project prices. New prices apply to spans received afterwards; stored costs are not recomputed (D-T-5).`

## Commands

```bash
cargo test -p cetra-server model_prices
```

```bash
cargo test -p cetra-server prices
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

- The five project routes work as § 4 says, with the 11 route tests and the 7 validation tests.
- The rule-check message names the settings page.
- OpenAPI, the generated TypeScript schema, the fixtures and the manifest are regenerated and committed.
- `just check` is green.

**Commit:** `server: add project model price routes`
