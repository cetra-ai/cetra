# T2 — Migration and the `meta::model_prices` store

**Commit subject:** `server: store custom model prices in meta.db`

Adds the `model_prices` table (spec 04 § Pricing as amended by D-T-2) and its store. No route and no change to pricing yet: T3 loads the rows.

## Read first

- README § Design → Model, § Migration number, D-T-2, D-T-4, D-T-8.
- `docs/spec/04-metadata-db.md` § Pricing.
- `crates/server/src/meta/mod.rs`: `MetaDb::write`, `MetaDb::reader`, `MetaError` (`Conflict`, `Corrupt`), `unique_or`, `test_db`, and the tests `open_twice_is_idempotent`, `open_existing_checks_the_file_and_the_version`, `schema_matches_the_snapshot`.
- `crates/server/src/meta/score_definitions.rs` (house style: a row struct, a private `sqlx::FromRow` record, a `row_from` decoder, a cap checked before the insert, `unique_or`, the `_tx` variants on `&mut SqliteConnection`), and its tests (how they create an org and a project).
- `crates/server/src/meta/orgs.rs` `insert_org`, `crates/server/src/meta/projects.rs` `insert_project`.
- `crates/server/src/admin.rs` test `schema_mismatch_names_the_versions`.
- `crates/otel/src/cost.rs`: `PriceRow`, `PriceScope`, `PriceTier` (T1).

## Files

| File | Change |
|---|---|
| `crates/server/migrations/meta/NNNN_model_prices.sql` | new (README § Migration number) |
| `crates/server/src/meta/model_prices.rs` | new |
| `crates/server/src/meta/mod.rs` | `pub(crate) mod model_prices;` (alphabetical in the module list) |
| `crates/server/tests/snapshots/meta_schema.snap` | regenerated |

## 1. Migration

`NNNN_model_prices.sql`, exactly:

```sql
CREATE TABLE model_prices (
  id TEXT PRIMARY KEY,
  org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
  project_id TEXT REFERENCES projects(id) ON DELETE CASCADE,
  model_name TEXT NOT NULL,
  match_pattern TEXT NOT NULL,
  provider TEXT,
  valid_from INTEGER NOT NULL DEFAULT 0,
  price_input REAL,
  price_output REAL,
  price_cache_read REAL,
  price_cache_write REAL,
  price_reasoning REAL,
  tiers TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX model_prices_key ON model_prices(
  org_id, coalesce(project_id, ''), match_pattern, coalesce(provider, ''), valid_from
);
CREATE INDEX model_prices_project ON model_prices(project_id) WHERE project_id IS NOT NULL;
```

`project_id` NULL is an org row; set, a project row of that org. The store does not check that the project belongs to the org: callers pass the route's own project (`access.project`), whose `org_id` is the org. `tiers` is the JSON of `Vec<PriceTier>` (`[{"min_input_tokens":…, "price_input":…, …}]`) or NULL. Times are nanoseconds.

### Schema version

The schema-version tests (`open_twice_is_idempotent`, `open_existing_checks_the_file_and_the_version` in `meta/mod.rs`, `schema_mismatch_names_the_versions` in `admin.rs`) read `crate::meta::expected_version()`, so they need no edit.

Regenerate the snapshot: `INSTA_UPDATE=always cargo test -p llmobs-server schema_matches_the_snapshot`. `git diff crates/server/tests/snapshots/meta_schema.snap` must show only an added block for `model_prices`, its two explicit indexes and one `sqlite_autoindex_model_prices_1`. Anything else: STOP and report.

## 2. `meta/model_prices.rs`

Module doc: `//! \`model_prices\` rows: custom prices of an org or a project (spec 04 § Pricing, D-T-2, D-T-4).`

Right after the module doc, add:

```rust
// Used by tests only until T3 loads the rows and T4/T5 serve them; T5 removes this line.
#![cfg_attr(not(test), allow(dead_code))]
```

`just lint` runs clippy on all targets with `-D warnings`, so code without a non-test caller fails without it (the same device as g-spine's first phases).

Imports: `llmobs_core::ids::{OrgId, ProjectId}`, `llmobs_otel::{PriceRow, PriceScope, PriceTier}`, `sqlx::{SqliteConnection, SqlitePool}`, `uuid::Uuid`, `super::{MetaError, unique_or}`.

### Constants

```rust
/// An org holds at most this many org rows (D-T-4).
pub(crate) const ORG_CAP: i64 = 500;
/// A project holds at most this many rows (D-T-4).
pub(crate) const PROJECT_CAP: i64 = 500;
/// The conflict reason of the unique index.
pub(crate) const DUPLICATE: &str = "a price with this pattern, provider and valid_from exists";
```

### Types (exactly these)

```rust
/// One stored custom price.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PriceRecord {
    /// UUIDv7.
    pub(crate) id: Uuid,
    /// Owning org.
    pub(crate) org_id: OrgId,
    /// The project of a project row; `None` for an org row.
    pub(crate) project_id: Option<ProjectId>,
    /// The price. `row.scope` is `Org` or `Project`, from `project_id`.
    pub(crate) row: PriceRow,
    /// Creation time, nanoseconds.
    pub(crate) created_at: i64,
    /// Last change, nanoseconds.
    pub(crate) updated_at: i64,
}

/// A budgeted judge rule, for the write guard (D-T-8).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BudgetedRule {
    /// Rule id.
    pub(crate) id: Uuid,
    /// Rule name, for the error message.
    pub(crate) name: String,
    /// The rule's project.
    pub(crate) project_id: ProjectId,
    /// The rule's evaluator.
    pub(crate) evaluator_id: Uuid,
}
```

A private record mirrors the columns (`#[derive(sqlx::FromRow)] struct PriceDbRecord { id: String, org_id: String, project_id: Option<String>, model_name: String, match_pattern: String, provider: Option<String>, valid_from: i64, price_input: Option<f64>, price_output: Option<f64>, price_cache_read: Option<f64>, price_cache_write: Option<f64>, price_reasoning: Option<f64>, tiers: Option<String>, created_at: i64, updated_at: i64 }`) and `fn record_from(record: PriceDbRecord) -> Result<PriceRecord, MetaError>` decodes it: ids parse as in `score_definitions.rs`'s `row_from` (a failure is `MetaError::Corrupt { table: "model_prices", column: "<column>" }`); `scope` is `PriceScope::Project` when `project_id` is set, else `PriceScope::Org`; `tiers` is `serde_json::from_str::<Vec<PriceTier>>` (failure: `Corrupt { column: "tiers" }`).

The column list used by every `SELECT` (put it in one `const COLUMNS: &str`):
`id, org_id, project_id, model_name, match_pattern, provider, valid_from, price_input, price_output, price_cache_read, price_cache_write, price_reasoning, tiers, created_at, updated_at`.

A private `fn tiers_json(row: &PriceRow) -> Result<Option<String>, MetaError>` serializes `row.tiers` (`serde_json::to_string`; an error is `Corrupt { column: "tiers" }`, which cannot happen for finite numbers).

### Functions (exactly these signatures)

```rust
/// Every row, ordered by `org_id`, then `id`. Read once at startup.
pub(crate) async fn list_all(pool: &SqlitePool) -> Result<Vec<PriceRecord>, MetaError>;

/// Every row of `org` (org rows and the rows of all its projects), ordered by `id`.
pub(crate) async fn list_org_all(pool: &SqlitePool, org: OrgId) -> Result<Vec<PriceRecord>, MetaError>;

/// The same read inside a write transaction.
pub(crate) async fn list_org_all_tx(conn: &mut SqliteConnection, org: OrgId) -> Result<Vec<PriceRecord>, MetaError>;

/// The org rows of `org` (`project_id IS NULL`), ordered by lowercased `model_name`, then `id`.
pub(crate) async fn list_org_rows(pool: &SqlitePool, org: OrgId) -> Result<Vec<PriceRecord>, MetaError>;

/// The row `id` of `org` in the given scope: `project` `None` finds an org row, `Some` a row of
/// that project. A row of another scope is `None`.
pub(crate) async fn get_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
    project: Option<ProjectId>,
    id: Uuid,
) -> Result<Option<PriceRecord>, MetaError>;

/// Inserts a row. The cap of its scope is checked first: past it is
/// `MetaError::Conflict("an org has at most 500 org prices")` or
/// `MetaError::Conflict("a project has at most 500 prices")`. A duplicate key is
/// `MetaError::Conflict(DUPLICATE)`.
pub(crate) async fn insert_tx(
    conn: &mut SqliteConnection,
    id: Uuid,
    org: OrgId,
    project: Option<ProjectId>,
    row: &PriceRow,
    now: i64,
) -> Result<PriceRecord, MetaError>;

/// Replaces every price column of the row `id` in that scope, and `updated_at`. `None` when no
/// such row. A duplicate key is `MetaError::Conflict(DUPLICATE)`.
pub(crate) async fn update_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
    project: Option<ProjectId>,
    id: Uuid,
    row: &PriceRow,
    now: i64,
) -> Result<Option<PriceRecord>, MetaError>;

/// Deletes the row `id` in that scope. False when no such row.
pub(crate) async fn delete_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
    project: Option<ProjectId>,
    id: Uuid,
) -> Result<bool, MetaError>;

/// Budgeted LLM-judge rules of `org`, or of one project of it, ordered by rule id (D-T-8).
pub(crate) async fn budgeted_rules_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
    project: Option<ProjectId>,
) -> Result<Vec<BudgetedRule>, MetaError>;
```

SQL:
- Scope filter used by `get_tx`, `update_tx`, `delete_tx`: `WHERE org_id = ? AND id = ? AND project_id IS ?` with `project.map(|p| p.to_string())` bound for the last `?` (SQLite's `IS` matches NULL to NULL and a value to an equal value).
- `list_org_rows`: `… WHERE org_id = ? AND project_id IS NULL ORDER BY lower(model_name), id`.
- `insert_tx`: the cap count is `SELECT COUNT(*) FROM model_prices WHERE org_id = ? AND project_id IS ?`; compare with `ORG_CAP` when `project` is `None`, else `PROJECT_CAP`. Then `INSERT INTO model_prices (<all columns>) VALUES (…)` with `created_at = updated_at = now`, `.map_err(|e| unique_or(e, DUPLICATE))`, then `get_tx` (a missing row after the insert is `Corrupt { table: "model_prices", column: "id" }`).
- `update_tx`: `UPDATE model_prices SET model_name = ?, match_pattern = ?, provider = ?, valid_from = ?, price_input = ?, price_output = ?, price_cache_read = ?, price_cache_write = ?, price_reasoning = ?, tiers = ?, updated_at = ? WHERE <scope filter>`, `unique_or(…, DUPLICATE)`; `rows_affected() == 0` → `Ok(None)`; else `get_tx`.
- `delete_tx`: `DELETE FROM model_prices WHERE <scope filter>`; `Ok(rows_affected() > 0)`.
- `budgeted_rules_tx`:

```sql
SELECT r.id, r.name, r.project_id, r.evaluator_id
FROM online_rules r
JOIN projects p ON p.id = r.project_id
JOIN evaluators e ON e.id = r.evaluator_id
WHERE p.org_id = ? AND r.budget_usd IS NOT NULL AND e.kind = 'llm_judge'
  AND (? IS NULL OR r.project_id = ?)
ORDER BY r.id
```

  Bind `org`, then `project` twice (`Option<String>`). Decode into `BudgetedRule` (ids as above; a bad id is `Corrupt { table: "online_rules", column: … }`).

The insert and update ignore `row.scope`: the scope comes from `project`. `row.valid_from` is stored as is.

### Tests (`#[cfg(test)] mod tests`, with `crate::meta::test_db()`)

Helpers: create an org (`insert_org`) with two projects (`insert_project`), and a second org with one project, inside `db.write`. A `fn price(name: &str, provider: Option<&str>) -> PriceRow` builds `PriceRow { scope: PriceScope::Org, model_name: name.into(), match_pattern: llmobs_otel::exact_pattern(name), provider: provider.map(Into::into), valid_from: 0, price_input: Some(1.0), price_output: Some(2.0), price_cache_read: None, price_cache_write: None, price_reasoning: None, tiers: None }` (`exact_pattern` is T1's; the server does not depend on `regex`).

1. `insert_get_round_trip_for_both_scopes`: insert an org row and a project row with tiers `[PriceTier { min_input_tokens: 200_000, price_input: Some(2.0), price_output: None, price_cache_read: None, price_cache_write: None, price_reasoning: None }]`; `get_tx` returns them with `scope` `Org` and `Project`, the same fields, `created_at == updated_at == now`.
2. `get_respects_the_scope`: the project row is `None` through `get_tx(org, None, id)` and through the other project; the org row is `None` through `get_tx(org, Some(project), id)`; any row is `None` through the second org.
3. `a_duplicate_key_is_a_conflict`: the same pattern, provider and `valid_from` twice in one org scope → `MetaError::Conflict(DUPLICATE)`; the same in two different projects, or once at org and once in a project, succeeds; a different `valid_from` or provider succeeds.
4. `a_null_provider_is_part_of_the_key`: `price("m", None)` twice in one scope → conflict.
5. `the_caps_hold`: insert 500 project rows (names `m0` … `m499`) in project A; the 501st → `Conflict("a project has at most 500 prices")`; a row in project B still succeeds; an org row still succeeds.
6. `update_replaces_every_price_column`: update a row with every price set and then with every price `None` and `tiers: None`; each `get_tx` matches; `updated_at` is the new `now`; `update_tx` of a missing id → `None`; an update onto another row's key → `Conflict(DUPLICATE)`.
7. `delete_reports_whether_a_row_went`: `true` then `false`; a row of another scope is not deleted.
8. `list_org_all_has_org_and_project_rows_in_id_order`, and `list_org_rows_has_org_rows_only_sorted_by_name` (`"b"`, `"A"`, `"c"` → `"A"`, `"b"`, `"c"`).
9. `list_all_covers_every_org`.
10. `rows_cascade_with_their_project_and_org`: delete a project (`DELETE FROM projects WHERE id = ?`) → its rows are gone, the org rows stay; delete the org → none left.
11. `budgeted_rules_lists_budgeted_judge_rules_only`: insert a connection, a judge evaluator and a check evaluator in project A and a judge evaluator in project B (copy the inserts used by the tests of `crates/server/src/meta/online_rules.rs`), then rules: A budgeted judge, A unbudgeted judge, A budgeted check (insert it directly with SQL, since the route refuses it), B budgeted judge. `budgeted_rules_tx(org, None)` → the two budgeted judge rules (A, B) in id order; `budgeted_rules_tx(org, Some(A))` → A's only; another org → empty.

## Commands

```bash
cargo test -p llmobs-server model_prices
```

```bash
just check
```

## Done when

- The migration, store and 11 tests are in; the snapshot is regenerated (§ Schema version: no version literal changes).
- Nothing reads the table yet (T3 does); the file carries the `dead_code` allowance.
- `just check` is green.

**Commit:** `server: store custom model prices in meta.db`
