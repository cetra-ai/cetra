# T3 — The `Prices` registry; every caller prices with the project's table

**Commit subject:** `server: price spans and judges with each project's table`

Adds `crate::prices::Prices` (README § Model, D-T-9): the built-in table, plus one layered table per org and per project that has custom rows, loaded from `meta.db` at startup and rebuilt per org on demand. `Ingest` holds it in place of its `RwLock<Arc<PriceTable>>`; `Ingest::replace_prices` and its `#[cfg(test)]` go (D-G-10's hook, D184). Every caller in README § Who prices with what switches to the project's (or org's) table. No route writes prices yet: tests insert rows through the T2 store and call `reload_org`.

## Read first

- README § Design (all), D-T-3, D-T-9.
- T1's `PriceTable::layered`; T2's `meta::model_prices` (`PriceRecord`, `list_all`, `list_org_all`).
- `crates/server/src/ingest/mod.rs`: `Ingest` (fields, `new`, `replace_prices`, `prices`, `accept_request`) and its tests `replace_prices_changes_what_accept_uses`, `expected`, `accept_body`, `builtin_prices`.
- `crates/server/src/ingest/fixtures_tests.rs` (the `replace_prices` call near the end).
- `crates/server/src/serve.rs` (where `PriceTable::new(builtin_price_rows())` and `Ingest::new` are called; `ServeError`), `crates/server/src/test_support/mod.rs` (the same two calls in `TestApp::open`; `Seeded`).
- `crates/server/src/deletion/mod.rs`: `drop_project_step` and the org drop step that calls `delete_org`.
- Every caller of `ingest.prices()`:

```bash
grep -rn "ingest.prices()\|replace_prices" crates/server/src
```

  Today: `eval/execute.rs` (`run_evaluator` and `reserve`, plus one test), `routes/backfills.rs` (`judge_cost`, plus one test), `routes/connections.rs` (the connection test), `routes/evaluators.rs` (the PATCH's `RuleChecks`, plus three tests), `routes/online_rules.rs` (`validate`), `ingest/fixtures_tests.rs`, `ingest/mod.rs`. If the list differs, STOP and report.

## Files

| File | Change |
|---|---|
| `crates/server/src/prices.rs` | new |
| `crates/server/src/lib.rs` | `pub(crate) mod prices;` between `pub(crate) mod meta;` and `pub(crate) mod retention;` |
| `crates/server/src/ingest/mod.rs` | `prices: Prices`; `prices()` returns `&Prices`; `replace_prices` removed; `accept_request` uses the project's table; tests |
| `crates/server/src/ingest/fixtures_tests.rs` | `set_builtin` |
| `crates/server/src/serve.rs` | `Prices::load` |
| `crates/server/src/test_support/mod.rs` | `Prices::new` |
| `crates/server/src/eval/execute.rs` | project tables; `reserve` takes the org |
| `crates/server/src/routes/{backfills,connections,evaluators,online_rules}.rs` | project or org tables |
| `crates/server/src/deletion/mod.rs` | `forget_project`, `forget_org` after a drop |

## 1. `prices.rs`

Module doc, then the allowance (T4 removes it: `builtin`, `reload_org` and `lock_writes` get non-test callers there):

```rust
//! The price tables of every org and project (spec 02 § Cost computation, D-T-3, D-T-9).
//!
//! The built-in snapshot is compiled once. An org with org rows gets a table of those rows
//! layered on the built-in one; a project with rows of its own gets a table of the org rows and
//! its rows layered on the built-in one. The ingest path reads a map under a read lock and never
//! touches `meta.db`.

// T4 removes this line, once the write routes call `reload_org` and `lock_writes`.
#![cfg_attr(not(test), allow(dead_code))]
```

Imports: `std::collections::HashMap`, `std::sync::{Arc, PoisonError, RwLock}`, `cetra_core::ids::{OrgId, ProjectId}`, `cetra_otel::{PriceError, PriceRow, PriceTable}`, `sqlx::SqlitePool`, `tokio::sync::{Mutex, MutexGuard}`, `crate::meta::MetaError`, `crate::meta::model_prices::{self, PriceRecord}`. (`cetra_otel` re-exports `PriceError`.)

### Types (exactly these)

```rust
/// Every price table of the process.
pub(crate) struct Prices {
    tables: RwLock<Tables>,
    /// Held by a price write from its transaction to its reload (README § Writes).
    writes: Mutex<()>,
}

struct Tables {
    builtin: Arc<PriceTable>,
    /// Orgs with org rows.
    orgs: HashMap<OrgId, Arc<PriceTable>>,
    /// Projects with rows of their own.
    projects: HashMap<ProjectId, Arc<PriceTable>>,
    /// The projects of each org that have an entry in `projects`, so a reload can remove them.
    org_projects: HashMap<OrgId, Vec<ProjectId>>,
}

/// The tables built from one org's rows.
pub(crate) struct OrgTables {
    /// `None` when the org has no org rows.
    pub(crate) org: Option<Arc<PriceTable>>,
    /// One entry per project with rows.
    pub(crate) projects: HashMap<ProjectId, Arc<PriceTable>>,
}
```

### Functions (exactly these signatures)

```rust
/// Builds `org`'s tables from its rows (org rows and every project's rows, `PriceRecord`s of
/// one org). The org rows come first in every project table, in the order given.
///
/// # Errors
///
/// [`PriceError`] when a table does not compile. Its index is a position in the table's rows,
/// not in `records`.
pub(crate) fn build_org(
    builtin: &Arc<PriceTable>,
    records: &[PriceRecord],
) -> Result<OrgTables, PriceError>;

impl Prices {
    /// The built-in table and no custom rows.
    pub(crate) fn new(builtin: PriceTable) -> Self;

    /// The built-in table and every stored row (startup). An org whose rows do not compile
    /// prices with the built-in table; its id is logged at error level (D-T-9).
    ///
    /// # Errors
    ///
    /// [`MetaError`] when the rows cannot be read.
    pub(crate) async fn load(reader: &SqlitePool, builtin: PriceTable) -> Result<Self, MetaError>;

    /// The built-in table.
    pub(crate) fn builtin(&self) -> Arc<PriceTable>;

    /// The table that prices spans of `project` in `org`: the project's, else the org's,
    /// else the built-in one.
    pub(crate) fn for_project(&self, org: OrgId, project: ProjectId) -> Arc<PriceTable>;

    /// The org's table, else the built-in one. For callers with no project (a connection test).
    pub(crate) fn for_org(&self, org: OrgId) -> Arc<PriceTable>;

    /// Reads `org`'s rows again and swaps its tables. Rows that do not compile drop the org's
    /// tables (logged, D-T-9).
    ///
    /// # Errors
    ///
    /// [`MetaError`] when the rows cannot be read; the old tables stay.
    pub(crate) async fn reload_org(&self, reader: &SqlitePool, org: OrgId) -> Result<(), MetaError>;

    /// Serializes price writes (README § Writes). Hold the guard across the write and
    /// [`Self::reload_org`].
    pub(crate) async fn lock_writes(&self) -> MutexGuard<'_, ()>;

    /// Drops the tables of a purged project.
    pub(crate) fn forget_project(&self, project: ProjectId);

    /// Drops the tables of a purged org and its projects.
    pub(crate) fn forget_org(&self, org: OrgId);

    /// Replaces the built-in table and rebuilds nothing else: tests only, before any custom
    /// row exists. Production never swaps the snapshot (D104).
    #[cfg(test)]
    pub(crate) fn set_builtin(&self, builtin: PriceTable);
}
```

Bodies:

- **`build_org`**: `org_rows: Vec<PriceRow>` = the `row` of each record with `project_id == None`, cloned, in order. `org = if org_rows.is_empty() { None } else { Some(Arc::new(PriceTable::layered(Arc::clone(builtin), org_rows.clone())?)) }`. Group the records with `project_id == Some(p)` by `p`, keeping their order (`HashMap<ProjectId, Vec<PriceRow>>`). For each project: `rows = org_rows.clone()` then extend with its rows; `projects.insert(p, Arc::new(PriceTable::layered(Arc::clone(builtin), rows)?))`.
- **`new`**: `Tables { builtin: Arc::new(builtin), orgs, projects, org_projects }` all empty; `writes: Mutex::new(())`.
- **`load`**: `let prices = Self::new(builtin);` `let records = model_prices::list_all(reader).await?;` group them by `org_id` (they are ordered by `org_id`, then `id`); for each org call the private `fn install(&self, org: OrgId, records: &[PriceRecord])` below. Return `prices`.
- **private `install`**: take `builtin` (clone of the `Arc`, under a read lock, released before building); `let built = build_org(&builtin, records);` then under the write lock: remove `orgs[org]`; remove every id of `org_projects.remove(&org)` from `projects`; on `Ok(tables)`: insert `tables.org` when `Some`, insert each project table and record the project ids in `org_projects[org]`; on `Err(_)`: `tracing::error!(org_id = %org, "custom prices do not compile; the org prices with the built-in table")` and install nothing. Locks: `self.tables.write().unwrap_or_else(PoisonError::into_inner)` (the same poison policy as `Ingest`).
- **`for_project`**: read lock; `projects.get(&project)`, else `orgs.get(&org)`, else `builtin`; `Arc::clone`.
- **`for_org`**: read lock; `orgs.get(&org)`, else `builtin`.
- **`reload_org`**: `let records = model_prices::list_org_all(reader, org).await?; self.install(org, &records); Ok(())`.
- **`lock_writes`**: `self.writes.lock().await`.
- **`forget_project`**: write lock; remove `projects[project]`; remove `project` from every `org_projects` vector (`retain`).
- **`forget_org`**: write lock; remove `orgs[org]`; for each id in `org_projects.remove(&org)`, remove it from `projects`.
- **`set_builtin`**: write lock; `builtin = Arc::new(builtin)`; clear `orgs`, `projects` and `org_projects` (a test calls it before inserting rows).

### Tests (`#[cfg(test)] mod tests` in `prices.rs`)

Helpers: `fn row(pattern: &str, scope: PriceScope, input: f64) -> PriceRow` (provider `None`, `valid_from` 0, `price_output: Some(1.0)`, other prices `None`, `tiers: None`); `fn record(org: OrgId, project: Option<ProjectId>, row: PriceRow) -> PriceRecord` (fresh `Uuid::now_v7()`, times 0); `fn input_price(table: &PriceTable, model: &str) -> Option<f64>` (`table.price_for(model, None, 1).and_then(|r| r.price_input)`). Use `OrgId::new_v7()` and `ProjectId::new_v7()`.

Built-in table for these tests: `PriceTable::new(vec![row("gpt-4o", PriceScope::Builtin, 2.5)])`.

1. `without_custom_rows_everything_is_builtin`: `Prices::new(…)`: `for_project` and `for_org` of any ids are pointer-equal to `builtin()` (`Arc::ptr_eq`).
2. `build_org_layers_org_rows_under_project_rows`: records: org row `gpt-4o` 1.0; project A row `gpt-4o` 0.5; project B none. `tables.org` prices `gpt-4o` at 1.0; `tables.projects[A]` at 0.5; B has no entry. A's table `rows()` has the org row then the project row.
3. `for_project_falls_back_to_the_org_then_builtin`: install the records of case 2 for org O (through `reload_org` in case 6, or call `install` directly here since the test is in the module): `for_project(O, A)` → 0.5, `for_project(O, B)` → 1.0, `for_project(other org, X)` → 2.5, `for_org(O)` → 1.0.
4. `an_invalid_row_leaves_the_org_on_builtin`: records with an org row whose pattern is `"("`; after `install`, `for_org(O)` is the built-in table; another org's tables are untouched.
5. `install_replaces_the_previous_tables`: install case 2, then install only project A's row: `for_org(O)` is built-in again, `for_project(O, A)` → 0.5; then install `[]`: everything of O is built-in.
6. `load_and_reload_read_meta_db` (`#[tokio::test]`, `crate::meta::test_db()`): create an org with two projects (T2's helpers), insert an org row and a project row with `model_prices::insert_tx` in `db.write`; `Prices::load(db.reader(), builtin)` prices them as case 3; delete the project row with `delete_tx`; `reload_org` → project A now gets the org price.
7. `forget_drops_tables`: after case 3's install, `forget_project(A)` → `for_project(O, A)` is the org table; `forget_org(O)` → built-in.
8. `for_project_is_cheap_to_share`: two `for_project(O, A)` calls return pointer-equal `Arc`s.

## 2. `ingest/mod.rs`

- Field `prices: RwLock<Arc<PriceTable>>` → `prices: crate::prices::Prices`.
- `Ingest::new(config: &IngestConfig, media: &MediaConfig, prices: crate::prices::Prices) -> Self`; store it.
- Delete `replace_prices` with its `#[cfg(test)]` attribute and its two doc lines ("Swaps the price table (D-G-10)…" and "The track that adds custom prices removes the `cfg`."), and update the `prices` field's doc if it mentions `RwLock`.
- `prices()` becomes:

```rust
/// The price tables (D-T-9). Callers pick the table of their project or org.
pub(crate) fn prices(&self) -> &crate::prices::Prices {
    &self.prices
}
```

- In `accept_request`, replace the read of `self.prices` with `let prices: Arc<PriceTable> = self.prices.for_project(project.org_id, project.id);`.
- Remove imports that become unused (`RwLock`, `PoisonError` if nothing else uses them; `cargo clippy` will say).
- Tests:
  - `builtin_prices()` stays.
  - Rename `replace_prices_changes_what_accept_uses` to `the_builtin_table_is_what_accept_uses_without_custom_rows`; replace its `replace_prices(…)` call with `app.state.ingest.prices().set_builtin(PriceTable::new(Vec::new()).expect("an empty table compiles"));`. The rest stays.
  - New `a_project_row_prices_that_project_only` (`#[tokio::test(flavor = "multi_thread")]`): `set_builtin(empty)`; create a second project in the seeded org (`meta::projects::insert_project` inside `app.state.meta.write`); insert a project row for the seeded project with pattern `".*"`, `price_input: Some(1.0)`, `price_output: Some(1.0)` (T2 `insert_tx`); `app.state.ingest.prices().reload_org(app.state.meta.reader(), seeded.project.org_id).await`. Accept `fixture("edge/cost-embedding/input.json")` into the seeded project: the embedding span has `cost_source` `computed` and `cost_input` equal to its `input_tokens` / 1e6. Accept the same body into the second project: `cost_source` `none`. (Find the span in `accepted.normalized.spans` by kind `Embedding`; read the `Span` field names in `cetra_core`.)
  - New `an_org_row_prices_every_project_of_the_org`: the same with an org row (`project` `None`): both projects are priced; a project of another org (create an org and a project) is not.
  - New `a_project_row_beats_an_org_row`: org row `.*` at 1.0 and project row `.*` at 2.0: the seeded project's `cost_input` is twice the other project's.

## 3. `ingest/fixtures_tests.rs`

Replace `app.state.ingest.replace_prices(prices.clone());` with `app.state.ingest.prices().set_builtin(prices.clone());`.

## 4. `serve.rs` and `test_support/mod.rs`

- `serve.rs`: after the `PriceTable::new(builtin_price_rows())` match (keep it and its `ServeError::Prices` arm), build the registry:

```rust
let prices = match crate::prices::Prices::load(meta.reader(), prices).await {
    Ok(prices) => prices,
    Err(error) => {
        release_storage(storage).await;
        meta.close().await;
        return Err(ServeError::Meta(error));
    }
};
```

  (`ServeError::Meta(#[from] MetaError)` exists; use the same release-then-close sequence as the arms around it.) `Ingest::new(&config.ingest, &config.media, prices)` is unchanged in form.
- `test_support/mod.rs`: wrap the built-in table: `crate::ingest::Ingest::new(&config.ingest, &config.media, crate::prices::Prices::new(prices))`. A test app starts with an empty `meta.db`, so `new` is right.

## 5. Callers

Each line below replaces `state.ingest.prices()` (an `Arc<PriceTable>` today) with a table of the same type:

| File, function | New expression |
|---|---|
| `eval/execute.rs`, `run_evaluator` (`let prices = …`) | `state.ingest.prices().for_project(project.org_id, project.id)` |
| `eval/execute.rs`, `reserve` | add a parameter `org: OrgId` after `state`; `state.ingest.prices().for_project(org, rule.project_id)`; at its one call site pass `project.org_id` (the `ProjectRow` in scope there) |
| `routes/backfills.rs`, `judge_cost` | `state.ingest.prices().for_project(org, rule.project_id)` |
| `routes/connections.rs`, the connection test | `state.ingest.prices().for_org(<the org id the handler passes to the connection lookup>)` |
| `routes/evaluators.rs`, the PATCH's `RuleChecks { prices: … }` | `state.ingest.prices().for_project(org, project)` (the ids passed to `check_against_store` just above) |
| `routes/online_rules.rs`, `validate` | `state.ingest.prices().for_project(org, project)` |
| tests in `eval/execute.rs`, `routes/backfills.rs`, `routes/evaluators.rs` | `app.state.ingest.prices().builtin()` |

Import `cetra_core::ids::OrgId` in `execute.rs` if it is not imported. If a variable named in the table does not exist where it is used, STOP and report.

Add one test in `routes/online_rules.rs`'s test module, next to the existing budget tests: `a_budget_on_a_local_model_is_accepted_once_the_project_prices_it`. Create a judge evaluator whose model is `"llama3.1:8b"` (no built-in price; use the existing helper that creates a judge evaluator with a given model — it takes `model: &'static str`). Creating a rule with `budget_usd: 5.0, budget_period: "day"` is `400` with field `budget_usd`. Insert a project row `{model_name: "llama3.1:8b", match_pattern: exact_pattern("llama3.1:8b"), provider: None, price_input: Some(0.0), price_output: Some(0.0)}` with T2's `insert_tx`, call `reload_org`, and create the rule again: `201`.

## 6. `deletion/mod.rs`

- `drop_project_step`: after the `meta.write` that deletes the project succeeds, next to `state.metrics_cache.clear_project(project);`, add:

```rust
{
    // A price reload that read the rows before the delete must not reinstall them (D-T-9).
    let _writes = state.ingest.prices().lock_writes().await;
    state.ingest.prices().forget_project(project);
}
```

- The org drop step (the one that calls `delete_org`): after its `meta.write` succeeds, next to the loop that clears the metrics cache, the same block with `forget_org(org)`.

No new test: the cascade is T2's test 10, and `forget_*` are case 7.

## Commands

```bash
cargo test -p cetra-server prices
```

```bash
cargo test -p cetra-server ingest
```

```bash
just check
```

## Done when

- `grep -rn "replace_prices\|RwLock<Arc<PriceTable>>" crates/server/src` prints nothing (code and doc comments).
- `prices.rs` carries the `dead_code` allowance; `just lint` passes.
- Every caller of README § Who prices with what uses `for_project` or `for_org`.
- The 8 registry tests, the 3 new ingest tests and the online-rule test pass; the golden fixture test passes with `set_builtin`.
- `just check` is green.

**Commit:** `server: price spans and judges with each project's table`
