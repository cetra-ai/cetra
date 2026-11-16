# J · M1 — Datasets: tables, store, routes, versions

Implementation plan for milestone M1 of track J. It has 4 sub-phases, **J1.1 to J1.4**, one commit each, done in order by one agent. **Haiku** implements each one with no context beyond this plan, the design doc and the files it names; **Sonnet** reviews the milestone. Nothing here may need a guess: if this plan and the code disagree (a name moved, a function changed, a test helper is missing), **STOP and report** instead of guessing. Each sub-phase ends with `just check` green.

**Prerequisites** (check before J1.1):

```bash
grep -n "pub(crate) fn unique_or" crates/server/src/meta/mod.rs
```

```bash
grep -n "fn score_definition_paths" crates/server/src/api/openapi.rs
```

```bash
grep -n "pub(crate) fn page_named" crates/server/src/routes/orgs.rs
```

Each must print one line. `docs/plans/j-datasets/00-design.md` must exist and list D-J-1 to D-J-19. Otherwise STOP.

**Read first** (every sub-phase):
- `AGENTS.md`, all of it.
- `docs/plans/j-datasets/00-design.md`: § Decisions (D-J-2, D-J-3, D-J-4, D-J-5, D-J-19), § Data model, § API contract, § Parallel work, § Fixed identifiers, § Conventions.
- This file up to § J1.1, then your own sub-phase.
- `docs/spec/04-metadata-db.md` § Datasets & experiments and `docs/spec/09-api.md` § Datasets & experiments (as amended by the J plan).

## What M1 is, and what it is not

M1 adds:
- one migration with **every** J table (`datasets`, `dataset_versions`, `dataset_items`, `experiments`, `experiment_items`), so J takes one migration number (D-J-1, D-J-19);
- the dataset store `crate::meta::datasets` (datasets, versions, item revisions, the version writer);
- validation in a new module `crate::datasets` (names, sizes, JSON Schema);
- `ProjectPerm::ManageDatasets` (D-J-5);
- the API types `crate::api::datasets` and the routes of every M1 row of the design's API table;
- OpenAPI, the access suite, `fixtures/api` examples for the example dataset `faq`;
- a `CHANGELOG.md` line and J's roadmap row set to **In progress**.

M1 does **not** add import, export or add-from-trace (M2), experiments routes, experiment scores, the SQL views or the deletion cascade (M3). The `experiments` and `experiment_items` tables exist after M1 but nothing writes them; `Dataset.experiment_count` reads `experiments` and is 0 until M3.

## Parallel work

M1 runs alone in J. Tracks S, T and N's M3 touch the same server files; keep every change additive and re-run the generators after a rebase:

| File | Rule |
|---|---|
| `crates/server/migrations/meta/NNNN_datasets.sql` | D-J-19: the next free number when J1.1 is implemented. If another branch lands the same number first, rename the file to the next free number at merge time and regenerate the schema snapshot. |
| `crates/server/tests/snapshots/meta_schema.snap` | Regenerate after a rebase (J1.1 § Schema snapshot). |
| `crates/server/src/api/openapi.rs`, `crates/server/openapi.json`, `web/src/api/schema.gen.ts` | Additive. After a rebase run `just openapi`; never hand-merge the two generated files. |
| `crates/server/tests/openapi.rs` (`OPERATIONS`, `TAGS`, `QUERY_OR_BODY`, the error branches, `pages`) | Additive. |
| `crates/server/src/http/access.rs` (route list), `routes/mod.rs`, `api/mod.rs`, `lib.rs`, `auth/roles.rs`, `http/access_suite.rs` | Additive lines. |
| `crates/server/src/api_examples.rs`, `crates/server/tests/api_examples.rs`, `fixtures/api/manifest.json`, `fixtures/api/README.md` | Additive; regenerate with `just api-examples` after a rebase. |
| `crates/server/Cargo.toml` | J1.2 moves `jsonschema` from `[dev-dependencies]` to `[dependencies]`. No version change. |
| `CHANGELOG.md`, `docs/roadmap.md` | Additive lines (J1.4). |

## Conventions for every sub-phase

- **Time**: `i64` nanoseconds in Rust and `meta.db`; JSON times use `cetra_core::time_serde` (`#[serde(with = "cetra_core::time_serde")] #[schema(value_type = String, format = DateTime)]`).
- **IDs**: `Uuid::now_v7()` for new rows, stored as hyphenated lowercase `TEXT` (`id.to_string()`). Trace and span ids are `BLOB`: bind `trace.as_bytes().to_vec()`, decode with `TraceId::from_slice(&bytes)` / `SpanId::from_slice(&bytes)` as `meta/eval_jobs.rs` does.
- **JSON columns**: `serde_json::to_string(&value)` on write (the workspace enables `preserve_order`, so key order is kept); `serde_json::from_str` on read, a failure being `MetaError::Corrupt { table, column }`.
- **Errors**: problem+json through `crate::http::error::ApiError`. A field error is `FieldError { field, message, position: None }` (`crate::api::common::FieldError`); a batch of them is `ApiError::BadRequest { detail: None, errors }`. `MetaError::Conflict(&'static str)` becomes a `409` through `?`.
- **Privacy**: logs carry dataset, item, project, user and key ids, counts and sizes; never a name, a description, a schema, an item value, or a JSON Schema error that quotes a value (AGENTS.md § Privacy). JSON Schema errors are reported as `"<instance path>: <keyword>"` only, as `cetra-eval`'s `check.rs` does.
- **No `unwrap`/`expect`** outside tests unless a comment next to it states the invariant.
- **Tests**: Rust tests sit in the module they test (`#[cfg(test)] mod tests`). HTTP tests use `TestApp` (`crate::test_support`), `#[tokio::test(flavor = "multi_thread")]`.
- **Commands**: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` before `just check` (the web check needs Node). Focused runs: `cargo test -p cetra-server meta::datasets`, `cargo test -p cetra-server datasets`, `cargo test -p cetra-server --test openapi`, `cargo test -p cetra-server --test api_examples`.

## Final layout after M1

```
crates/server/migrations/meta/NNNN_datasets.sql   every J table                          (J1.1)
crates/server/src/meta/datasets.rs                store                                  (J1.1)
crates/server/src/datasets/mod.rs                 validation                             (J1.2)
crates/server/src/api/datasets.rs                 API types                              (J1.2)
crates/server/src/auth/roles.rs                   + ProjectPerm::ManageDatasets           (J1.2)
crates/server/src/routes/datasets.rs              dataset routes (J1.3), item and version routes (J1.4)
crates/server/src/api/openapi.rs                  + dataset_paths                        (J1.3, J1.4)
fixtures/api/responses/datasets*.json, …          generated examples                     (J1.3, J1.4)
fixtures/api/requests/dataset-*.json              request examples                       (J1.3, J1.4)
```

## Shared reference

### Fixed identifiers

From the design's § Fixed identifiers. Before J1.3, check the ranges are free:

```bash
grep -rnE "8000-000000000(c|d)[0-9a-f]{2}" fixtures crates web sdks
```

It must print nothing. Otherwise STOP.

| Thing | Value |
|---|---|
| Dataset `faq` | `01900000-0000-7000-8000-000000000c01` |
| Items | `01900000-0000-7000-8000-000000000d01`, `…0d02`, `…0d03` |
| Revisions | `01900000-0000-7000-8000-000000000d11` (item `…0d01`, v1), `…0d12` (`…0d02`, v1, closed at v2), `…0d13` (`…0d03`, v1), `…0d14` (`…0d02`, v2) |

### The example dataset `faq`

Written by `seed_example_faq_dataset` (J1.3) through the store, so every generated file is the real handlers' output.

- Dataset: name `faq`, description `Support questions and expected answers`, `input_schema`:
  `{"type":"object","required":["question"],"properties":{"question":{"type":"string"}}}`, no `expected_schema`.
- Version 1, description `add 3 items`, `created_by` absent:
  - `…0d01` / `…0d11`: input `{"question":"How do I reset my password?"}`, expected `{"answer":"Use the Forgot password link on the login page."}`, metadata `{"topic":"account"}`.
  - `…0d02` / `…0d12`: input `{"question":"Can I export my traces?"}`, expected `{"answer":"Yes, as CSV."}`, metadata `{"topic":"data"}`.
  - `…0d03` / `…0d13`: input `{"question":"Which plans include SSO?"}`, no expected, metadata `{}`, `source_trace_id` `a1000000000000000000000000000001`, `source_span_id` `b100000000000011`.
- Version 2, description `edit item 01900000-0000-7000-8000-000000000d02`: `…0d02` / `…0d14` with expected `{"answer":"Yes, as JSONL or CSV."}`, the rest unchanged.

M2's and M3's fixtures read this dataset: after M1 it is at `current_version` 2 with 3 live items.

### Interfaces for M2 and M3

M2 (import, export, add-from-trace) and M3 (experiments) call these M1 names. Keep them exactly; a later milestone that needs a change says so in its own plan.

| Name | Where | Use |
|---|---|---|
| `DatasetKey::parse`, `get_dataset`, `get_dataset_tx` | `meta::datasets` | resolve `{dataset}` (id or name) in a route or a write transaction |
| `DatasetRow` (`current_version`, `input_schema`, `expected_schema`, …) | `meta::datasets` | the stored dataset |
| `NewRevision` (with optional `source_trace_id`, `source_span_id`), `VersionChange`, `write_version` | `meta::datasets` | every mutation, including an import (inserts only) and add-from-trace (one insert with the source ids); the 50 000 live-item cap is enforced inside |
| `list_items(pool, dataset, version, after, limit)` | `meta::datasets` | export pages and the experiment's items (`limit + 1` rule) |
| `get_item`, `get_live_item_tx` | `meta::datasets` | one revision |
| `ItemValues`, `Schemas::of`, `check_item(prefix, item, schemas)`, `MAX_ITEM_BYTES`, `check_name` | `crate::datasets` | compile the schemas once per request, check each item |
| `find`, `created_by`, `version_of`, `item_json`, `dataset_json`, `DatasetPath` | `routes::datasets` (`pub(crate)`) | route plumbing |
| `UpdateItemRequest.expected_output: Option<Option<Value>>` | `api::datasets` | absent keeps, `null` clears |
| `any_json` | `api::common` | the one OpenAPI form of an "any JSON" field (utoipa `AnyValue`): `#[schema(schema_with = any_json)]` on every input, expected output, output and preview row; never `value_type = Object` for those (D-J-2) |
| tag `datasets`, `fn dataset_paths`, `path_string("dataset", …)` | `api::openapi` | M2's operations go in `dataset_paths` |
| `concrete_path`'s `{dataset}` → `faq` and `{item_id}` replacements | `http/access_suite.rs` | M2's and M3's routes need no new replacement for these segments |
| `TestApp::seed_example_faq_dataset`, `TestApp::write_key`, `EXAMPLE_DATASET` | `test_support`, `api_examples.rs` | fixtures and tests |

---

## J1.1 — Migration and the `meta::datasets` store

**Commit subject:** `server: add the dataset and experiment tables and the dataset store`

### Read first

- `crates/server/src/meta/mod.rs`: `MetaDb::write`, `MetaDb::reader`, `MetaError` (`Conflict`, `Corrupt`), `unique_or`, `test_db`, the module list, and the tests `open_twice_is_idempotent`, `open_existing_checks_the_file_and_the_version`, `schema_matches_the_snapshot`.
- `crates/server/src/meta/score_definitions.rs` (house style: a row struct, a private `sqlx::FromRow` record, `row_from`, `corrupt`, `parse_uuid`, a cap checked before the insert, `unique_or`, `_tx` variants on `&mut SqliteConnection`) and its tests (`org`, `project` helpers).
- `crates/server/src/meta/eval_jobs.rs`: how trace and span ids are bound and decoded (`trace_bytes`, `TraceId::from_slice`).
- `crates/server/migrations/meta/0014_backfills.sql` (SQL style).

### Files

| File | Change |
|---|---|
| `crates/server/migrations/meta/NNNN_datasets.sql` | new; `NNNN` = the highest number in that directory plus one, four digits (`0016` if `0015_email_verified_via.sql` is the last) |
| `crates/server/src/meta/datasets.rs` | new |
| `crates/server/src/meta/mod.rs` | `pub(crate) mod datasets;` in alphabetical order (after `connections`) |
| `crates/server/tests/snapshots/meta_schema.snap` | regenerated |

### 1. Migration

`NNNN_datasets.sql`, exactly the SQL of the design's § Data model (the five `CREATE TABLE`s, their seven `CREATE INDEX`es and the `CREATE UNIQUE INDEX dataset_items_one_live`), preceded by one comment line:

```sql
-- Track J: datasets, versions, item revisions, experiments and their results (D-J-1, spec 04).
```

Copy the SQL from `docs/plans/j-datasets/00-design.md` § Data model character for character (comments included). Do not add or remove a column.

### 2. Schema snapshot

The version tests read `crate::meta::expected_version()`, so they need no edit. Regenerate the snapshot:

```bash
INSTA_UPDATE=always cargo test -p cetra-server schema_matches_the_snapshot
```

`git diff crates/server/tests/snapshots/meta_schema.snap` must show only added blocks for the five tables, their eight explicit indexes (seven plus `dataset_items_one_live`) and the automatic indexes SQLite creates for the primary keys and the `UNIQUE(project_id, name)` (`sqlite_autoindex_…`). Anything else: STOP.

### 3. `meta/datasets.rs`

Module doc: `//! \`datasets\`, \`dataset_versions\` and \`dataset_items\` rows (spec 04 § Datasets & experiments, D-J-2, D-J-3, D-J-4).`

Imports: `cetra_core::ids::{ProjectId, SpanId, TraceId}`, `serde_json::{Map, Value}`, `sqlx::{SqliteConnection, SqlitePool}`, `uuid::Uuid`, `super::{MetaError, unique_or}`.

#### Constants

```rust
/// A project holds at most this many datasets (D-J-4).
pub(crate) const PROJECT_CAP: i64 = 1_000;
/// The latest version of a dataset holds at most this many live items (D-J-4).
pub(crate) const LIVE_ITEM_CAP: i64 = 50_000;
/// Conflict reasons.
pub(crate) const DUPLICATE_NAME: &str = "a dataset with this name already exists";
pub(crate) const TOO_MANY_DATASETS: &str = "a project has at most 1000 datasets";
pub(crate) const TOO_MANY_ITEMS: &str = "a dataset holds at most 50000 items";
```

#### Types (exactly these)

```rust
/// One stored dataset, with its counts.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DatasetRow {
    /// UUIDv7.
    pub(crate) id: Uuid,
    /// Owning project.
    pub(crate) project_id: ProjectId,
    /// Unique in the project; never parses as a UUID (D-J-4).
    pub(crate) name: String,
    /// Free text.
    pub(crate) description: Option<String>,
    /// JSON Schema text for `input`, when set.
    pub(crate) input_schema: Option<String>,
    /// JSON Schema text for `expected_output`, when set.
    pub(crate) expected_schema: Option<String>,
    /// Latest version; 0 for a new dataset.
    pub(crate) current_version: i64,
    /// Live items at `current_version`.
    pub(crate) item_count: i64,
    /// Experiments whose `dataset_id` is this dataset.
    pub(crate) experiment_count: i64,
    /// Creation time, nanoseconds.
    pub(crate) created_at: i64,
    /// Last change, nanoseconds.
    pub(crate) updated_at: i64,
}

/// The editable columns of a dataset. Insert takes them; update replaces them all.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DatasetFields {
    pub(crate) name: String,
    pub(crate) description: Option<String>,
    pub(crate) input_schema: Option<String>,
    pub(crate) expected_schema: Option<String>,
}

/// How a route names a dataset: `{dataset}` is an id when it parses as a UUID, else a name (D-J-4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DatasetKey {
    Id(Uuid),
    Name(String),
}

impl DatasetKey {
    /// `Id` when `text` parses as a UUID, else `Name(text)` unchanged.
    pub(crate) fn parse(text: &str) -> Self;
}

/// One `dataset_versions` row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VersionRow {
    pub(crate) version: i64,
    pub(crate) description: String,
    /// A user id, or `key:<key id>`.
    pub(crate) created_by: Option<String>,
    pub(crate) created_at: i64,
}

/// One item revision. `Debug` is hand-written (below): it prints ids, versions and sizes, never content.
#[derive(Clone, PartialEq)]
pub(crate) struct ItemRow {
    pub(crate) revision_id: Uuid,
    /// Stable across revisions.
    pub(crate) item_id: Uuid,
    pub(crate) dataset_id: Uuid,
    pub(crate) input: Value,
    pub(crate) expected_output: Option<Value>,
    pub(crate) metadata: Map<String, Value>,
    pub(crate) source_trace_id: Option<TraceId>,
    pub(crate) source_span_id: Option<SpanId>,
    pub(crate) valid_from_version: i64,
    pub(crate) valid_to_version: Option<i64>,
    pub(crate) created_at: i64,
}

/// A revision to insert. Its `valid_from_version` is the new version. `Debug` is hand-written.
#[derive(Clone, PartialEq)]
pub(crate) struct NewRevision {
    pub(crate) revision_id: Uuid,
    pub(crate) item_id: Uuid,
    pub(crate) input: Value,
    pub(crate) expected_output: Option<Value>,
    pub(crate) metadata: Map<String, Value>,
    pub(crate) source_trace_id: Option<TraceId>,
    pub(crate) source_span_id: Option<SpanId>,
}

/// One mutation of a dataset: the items whose live revision closes, the revisions to insert.
/// An edit closes the item and inserts its new revision; a delete only closes; an add only inserts.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VersionChange {
    pub(crate) close: Vec<Uuid>,
    pub(crate) insert: Vec<NewRevision>,
    pub(crate) description: String,
    pub(crate) created_by: Option<String>,
}
```

`DatasetKey::parse` is `text.parse::<Uuid>().map_or_else(|_| Self::Name(text.to_owned()), Self::Id)`.

`VersionChange` derives `Debug` through `NewRevision`'s hand-written one. Write `impl fmt::Debug` for `ItemRow` and `NewRevision` so a stray `{:?}` can never log item content (AGENTS.md § Privacy):

```rust
impl fmt::Debug for ItemRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ItemRow")
            .field("revision_id", &self.revision_id)
            .field("item_id", &self.item_id)
            .field("dataset_id", &self.dataset_id)
            .field("valid_from_version", &self.valid_from_version)
            .field("valid_to_version", &self.valid_to_version)
            .field("bytes", &json_bytes(&self.input, self.expected_output.as_ref(), &self.metadata))
            .finish_non_exhaustive()
    }
}
```

and the same for `NewRevision` (`revision_id`, `item_id`, `bytes`). `fn json_bytes(input: &Value, expected: Option<&Value>, metadata: &Map<String, Value>) -> usize` is a private helper summing `serde_json::to_string(…).map_or(0, |s| s.len())` of the three. Add `use std::fmt;`. A test `debug_prints_no_content` formats an `ItemRow` whose input is `"canary-debug"` and asserts the text has no `canary-debug`.

Private records mirror the columns (`DatasetRecord { id, project_id, name, description, input_schema, expected_schema, current_version, created_at, updated_at, item_count, experiment_count }` with `String`/`Option<String>`/`i64`; `VersionRecord`; `ItemRecord { revision_id, item_id, dataset_id, input, expected_output, metadata, source_trace_id: Option<Vec<u8>>, source_span_id: Option<Vec<u8>>, valid_from_version, valid_to_version, created_at }`). Decoders `dataset_from`, `version_from`, `item_from` return `MetaError::Corrupt { table, column }` on a bad UUID, JSON or id (`metadata` must decode as a JSON object: `serde_json::from_str::<Map<String, Value>>`).

The dataset `SELECT` used by every dataset read, in one `const DATASET_SELECT: &str`:

```sql
SELECT d.id, d.project_id, d.name, d.description, d.input_schema, d.expected_schema,
       d.current_version, d.created_at, d.updated_at,
       (SELECT COUNT(*) FROM dataset_items i
         WHERE i.dataset_id = d.id AND i.valid_to_version IS NULL) AS item_count,
       (SELECT COUNT(*) FROM experiments e WHERE e.dataset_id = d.id) AS experiment_count
FROM datasets d
```

The item columns in `const ITEM_COLUMNS: &str`: `revision_id, item_id, dataset_id, input, expected_output, metadata, source_trace_id, source_span_id, valid_from_version, valid_to_version, created_at`.

The visibility predicate of version *v* (D-J-3), in `const IN_VERSION: &str`: `valid_from_version <= ?1 AND (valid_to_version IS NULL OR valid_to_version > ?1)`. Use numbered parameters (`?1`, `?2`, …) in the queries that use it, so `v` is bound once.

#### Functions (exactly these signatures)

```rust
/// Every dataset of `project`, ordered by `lower(name)`, then `id`.
pub(crate) async fn list_datasets(pool: &SqlitePool, project: ProjectId)
    -> Result<Vec<DatasetRow>, MetaError>;

/// The dataset of `project` named by `key`.
pub(crate) async fn get_dataset(pool: &SqlitePool, project: ProjectId, key: &DatasetKey)
    -> Result<Option<DatasetRow>, MetaError>;

/// The same read inside a write transaction.
pub(crate) async fn get_dataset_tx(conn: &mut SqliteConnection, project: ProjectId, key: &DatasetKey)
    -> Result<Option<DatasetRow>, MetaError>;

/// Inserts a dataset at version 0. Past [`PROJECT_CAP`] is `Conflict(TOO_MANY_DATASETS)`;
/// a duplicate name is `Conflict(DUPLICATE_NAME)`.
pub(crate) async fn insert_dataset(conn: &mut SqliteConnection, id: Uuid, project: ProjectId,
    fields: DatasetFields, now: i64) -> Result<DatasetRow, MetaError>;

/// Replaces the editable columns and `updated_at`. `None` when no such dataset in `project`.
/// A duplicate name is `Conflict(DUPLICATE_NAME)`.
pub(crate) async fn update_dataset(conn: &mut SqliteConnection, project: ProjectId, id: Uuid,
    fields: DatasetFields, now: i64) -> Result<Option<DatasetRow>, MetaError>;

/// Deletes the dataset; its versions and items go by cascade, its experiments keep their rows
/// with `dataset_id` NULL. False when no such dataset in `project`.
pub(crate) async fn delete_dataset(conn: &mut SqliteConnection, project: ProjectId, id: Uuid)
    -> Result<bool, MetaError>;

/// Writes one version (design § Data model, "Writing a version") and returns its number.
/// Past [`LIVE_ITEM_CAP`] live items afterwards is `Conflict(TOO_MANY_ITEMS)`; the caller's
/// transaction then rolls back.
pub(crate) async fn write_version(conn: &mut SqliteConnection, dataset: Uuid,
    change: VersionChange, now: i64) -> Result<i64, MetaError>;

/// Versions of `dataset`, newest first; `before` is the cursor (versions strictly lower).
/// Returns at most `limit + 1` rows; the caller uses the extra row to know there is a next page.
pub(crate) async fn list_versions(pool: &SqlitePool, dataset: Uuid, before: Option<i64>, limit: u32)
    -> Result<Vec<VersionRow>, MetaError>;

/// The revisions in `version`, ordered by `item_id`, with `item_id > after` when set.
/// Returns at most `limit + 1` rows.
pub(crate) async fn list_items(pool: &SqlitePool, dataset: Uuid, version: i64,
    after: Option<Uuid>, limit: u32) -> Result<Vec<ItemRow>, MetaError>;

/// The same page of the live revisions (`valid_to_version IS NULL`), inside a write transaction.
pub(crate) async fn live_items_tx(conn: &mut SqliteConnection, dataset: Uuid,
    after: Option<Uuid>, limit: u32) -> Result<Vec<ItemRow>, MetaError>;

/// The revision of `item` in `version`.
pub(crate) async fn get_item(pool: &SqlitePool, dataset: Uuid, item: Uuid, version: i64)
    -> Result<Option<ItemRow>, MetaError>;

/// The live revision of `item`, inside a write transaction.
pub(crate) async fn get_live_item_tx(conn: &mut SqliteConnection, dataset: Uuid, item: Uuid)
    -> Result<Option<ItemRow>, MetaError>;
```

#### SQL and rules

- `list_datasets`: `DATASET_SELECT WHERE d.project_id = ? ORDER BY lower(d.name), d.id`.
- `get_dataset` / `get_dataset_tx`: `DATASET_SELECT` plus `WHERE d.project_id = ? AND d.id = ?` for `Id`, `WHERE d.project_id = ? AND d.name = ?` for `Name`. Build the text with a private `fn dataset_where(key: &DatasetKey) -> String` (`format!("{DATASET_SELECT} WHERE d.project_id = ? AND d.{} = ?", if Id { "id" } else { "name" })`) and bind the id text or the name; both variants use it.
- `insert_dataset`: `SELECT COUNT(*) FROM datasets WHERE project_id = ?`; `>= PROJECT_CAP` → `Conflict(TOO_MANY_DATASETS)`. Then `INSERT INTO datasets (id, project_id, name, description, input_schema, expected_schema, current_version, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, 0, ?, ?)` with `.map_err(|e| unique_or(e, DUPLICATE_NAME))`, then `get_dataset_tx(…, &DatasetKey::Id(id))` (`None` → `Corrupt { table: "datasets", column: "id" }`).
- `update_dataset`: `UPDATE datasets SET name = ?, description = ?, input_schema = ?, expected_schema = ?, updated_at = ? WHERE project_id = ? AND id = ?`; `rows_affected() == 0` → `Ok(None)`; else read back as in insert.
- `delete_dataset`: `DELETE FROM datasets WHERE project_id = ? AND id = ?`; `rows_affected() > 0`.
- `write_version`, in this order, all on `conn`:
  1. `SELECT current_version FROM datasets WHERE id = ?` (`None` → `Corrupt { table: "datasets", column: "id" }`). `v = current_version + 1`.
  2. For each id in `change.close`: `UPDATE dataset_items SET valid_to_version = ? WHERE dataset_id = ? AND item_id = ? AND valid_to_version IS NULL`. A close that affects 0 rows is `Corrupt { table: "dataset_items", column: "item_id" }` (callers close only live items they just read in the same transaction).
  3. For each revision in `change.insert`: `INSERT INTO dataset_items (revision_id, item_id, dataset_id, input, expected_output, metadata, source_trace_id, source_span_id, valid_from_version, valid_to_version, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?)`.
  4. `SELECT COUNT(*) FROM dataset_items WHERE dataset_id = ? AND valid_to_version IS NULL`; `> LIVE_ITEM_CAP` → `Err(Conflict(TOO_MANY_ITEMS))`.
  5. `INSERT INTO dataset_versions (dataset_id, version, description, created_by, created_at) VALUES (?, ?, ?, ?, ?)`.
  6. `UPDATE datasets SET current_version = ?, updated_at = ? WHERE id = ?`.
  7. `Ok(v)`.
- `list_versions`: `SELECT version, description, created_by, created_at FROM dataset_versions WHERE dataset_id = ?1 AND (?2 IS NULL OR version < ?2) ORDER BY version DESC LIMIT ?3`, binding `limit + 1`.
- `list_items`: `SELECT ITEM_COLUMNS FROM dataset_items WHERE dataset_id = ?2 AND <IN_VERSION> AND (?3 IS NULL OR item_id > ?3) ORDER BY item_id LIMIT ?4` with `?1 = version`, `?3 = after.map(|id| id.to_string())`, `?4 = limit + 1`. Item ids are UUIDv7 text, so text order is creation order.
- `live_items_tx`: the same with `valid_to_version IS NULL` instead of `IN_VERSION`.
- `get_item`: `… WHERE dataset_id = ?2 AND item_id = ?3 AND <IN_VERSION>` (at most one row by construction).
- `get_live_item_tx`: `… WHERE dataset_id = ? AND item_id = ? AND valid_to_version IS NULL`.

Build the SQL strings with `format!` from the constants only where a constant is spliced, and wrap them in `sqlx::AssertSqlSafe(sql)` with the comment `// Only constants are spliced; every value is bound.` as `meta/saved_views.rs` does. Prefer full literal strings where no splice is needed.

### Tests (`meta/datasets.rs`, `#[cfg(test)] mod tests`)

Helpers: `org` and `project` exactly as in `meta/score_definitions.rs` tests; `fn fields(name: &str) -> DatasetFields` (no description, no schemas); `fn revision(item: Uuid, input: Value) -> NewRevision` (new `revision_id`, metadata empty, no source); `const NOW: i64 = 1_000;`.

1. `insert_get_list_and_delete` — insert `faq` and `Zeta` and `alpha`; `list_datasets` returns `alpha`, `faq`, `Zeta` (lowercase order); `get_dataset` by `Id` and by `Name("faq")` both find it with `current_version == 0`, `item_count == 0`, `experiment_count == 0`; `delete_dataset` is true, then false, and `get_dataset` is `None`.
2. `duplicate_name_is_a_conflict` — a second `faq` in the same project is `MetaError::Conflict(DUPLICATE_NAME)`; the same name in a second project is fine. `update_dataset` renaming `alpha` to `faq` is the same conflict.
3. `project_cap` — insert `PROJECT_CAP` datasets named `d0000`…`d0999` in one write (a loop inside one `db.write`), then one more is `Conflict(TOO_MANY_DATASETS)`.
4. `versions_follow_the_validity_ranges` — v1 adds items A and B; v2 closes A and inserts A′ (same `item_id`, new input); v3 closes B. Then: `list_items(v1)` is A, B with the v1 inputs; `list_items(v2)` is A′, B; `list_items(v3)` is A′ only; `list_items(v0)` is empty; `get_item(A, 1)` is the old revision and `get_item(A, 2)` the new one; `get_item(B, 3)` is `None`; `get_dataset` has `current_version == 3`, `item_count == 1`; `list_versions(None, 10)` is 3, 2, 1 with their descriptions and `created_by`.
5. `paging` — 5 items in one version; `list_items(v1, None, 2)` returns 3 rows (limit + 1); `after` = the second row's `item_id` returns the next rows; `list_versions(Some(3), 10)` returns versions 2 and 1 only.
6. `live_item_cap` — set up a dataset with `LIVE_ITEM_CAP` live items (one `write_version` with 50 000 small revisions, `input = json!(i)`), then `write_version` adding one more is `Conflict(TOO_MANY_ITEMS)`, and in the same test `get_dataset` still shows `current_version == 1` (the failed write rolled back). A close of one item plus an insert of a new one in the same change is accepted (the count is taken after both).
7. `source_ids_round_trip` — an inserted revision with `source_trace_id` `a1000000000000000000000000000001` and `source_span_id` `b100000000000011` reads back equal.
8. `one_live_revision_per_item` — after v1 inserts item A, a direct `sqlx::query` inserting a second revision of A with `valid_to_version` NULL fails with a unique-constraint error (`dataset_items_one_live`); and a `write_version` whose `insert` holds A twice is an error and leaves `current_version` at 1 (the transaction rolls back).
9. `delete_keeps_experiments_with_null_dataset` — insert a dataset, then insert one `experiments` row directly with `sqlx::query` (`id`, `project_id`, `dataset_id` = the dataset, `dataset_name`, `dataset_version` 0, `name`, `item_count` 0, `status` `'running'`, `created_at` NOW); `get_dataset` shows `experiment_count == 1`; `delete_dataset`; `SELECT dataset_id FROM experiments` is `NULL`.

### Done when

- The migration and the store exist with exactly the names above; `meta_schema.snap` is regenerated with only the additions.
- `cargo test -p cetra-server meta::` passes; `just check` is green.

---

## J1.2 — Validation, the permission and the API types

**Commit subject:** `server: add dataset validation, ManageDatasets and the dataset API types`

### Read first

- J1.1's `meta/datasets.rs`.
- `crates/eval/src/check.rs`: `compile_schema` and `json_schema` (how the workspace compiles Draft 2020-12 offline and reports `instance_path: keyword`).
- `crates/server/src/auth/roles.rs`: `ProjectPerm`, `min_role`, `key_scope` and the test that asserts each permission (around the `ProjectPerm::Annotate` asserts).
- `crates/server/src/api/score_definitions.rs` and `crates/server/src/api/account.rs` (`UpdateProjectRequest`, `optional_retention`: how a PATCH tells an absent field from `null`).
- `crates/server/src/api/common.rs`: `Page`, `Limit`, `FieldError`.
- `crates/server/src/api/payload.rs` around line 71: the `"json"` branch builds an any-JSON schema with `ObjectBuilder::new().schema_type(SchemaType::AnyValue)`.

### Files

| File | Change |
|---|---|
| `crates/server/Cargo.toml` | move `jsonschema.workspace = true` from `[dev-dependencies]` to `[dependencies]` (alphabetical); the root `Cargo.toml` is unchanged |
| `crates/server/src/datasets/mod.rs` | new |
| `crates/server/src/lib.rs` | `pub(crate) mod datasets;` (alphabetical, after `crypto`) |
| `crates/server/src/auth/roles.rs` | `ProjectPerm::ManageDatasets`, `min_role`, `key_scope`, test asserts |
| `crates/server/src/api/datasets.rs` | new |
| `crates/server/src/api/mod.rs` | `pub mod datasets;` (alphabetical) |
| `crates/server/src/api/common.rs` | `pub(crate) fn any_json` (§ 3) |

### 1. `ProjectPerm::ManageDatasets`

Add after `ManageAlerts`:

```rust
    /// Datasets and experiments: create, edit, delete, items, import, add from trace, experiment
    /// results (D-J-5). Member and above; a `write` API key; a read-write personal access token.
    ManageDatasets,
```

`min_role`: add `Self::ManageDatasets` to the `Role::Member` arm. `key_scope`: add it to the `Some(KeyScope::Write)` arm (`Self::Annotate | Self::Delete | Self::ManageDatasets`). In the roles test, after the `ManageAlerts` asserts:

```rust
        // Datasets and experiments: member and above, and a `write` key (D-J-5).
        assert_eq!(ProjectPerm::ManageDatasets.min_role(), Role::Member);
        assert_eq!(ProjectPerm::ManageDatasets.key_scope(), Some(KeyScope::Write));
```

If the compiler reports another exhaustive `match` on `ProjectPerm` elsewhere (for example in `http/access_suite.rs` or the web role table), add the variant there with the same meaning and say so in your report.

### 2. `datasets/mod.rs`

Module doc: `//! Dataset validation: names, sizes, JSON Schemas (D-J-2, D-J-4). No database access.`

Until J1.3 calls it, add right after the doc: `#![cfg_attr(not(test), allow(dead_code))] // J1.3 removes this line.`

```rust
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::api::common::FieldError;

/// Longest name, in characters, after trimming.
pub(crate) const MAX_NAME_CHARS: usize = 200;
/// Longest description, in characters.
pub(crate) const MAX_DESCRIPTION_CHARS: usize = 2_000;
/// Longest JSON Schema, serialized, in bytes.
pub(crate) const MAX_SCHEMA_BYTES: usize = 64 * 1024;
/// Largest item: `input` + `expected_output` + `metadata`, serialized, in bytes.
pub(crate) const MAX_ITEM_BYTES: usize = 1024 * 1024;
/// Items in one batch add.
pub(crate) const MAX_BATCH_ITEMS: usize = 1_000;
/// Schema failures listed when a schema change is refused.
pub(crate) const MAX_SCHEMA_FAILURES: usize = 20;

/// An item's values, before or after storage. `Debug` is hand-written: sizes only.
#[derive(Clone, PartialEq)]
pub(crate) struct ItemValues {
    pub(crate) input: Value,
    pub(crate) expected_output: Option<Value>,
    pub(crate) metadata: Map<String, Value>,
}

/// The compiled schemas of one dataset.
pub(crate) struct Schemas {
    input: Option<jsonschema::Validator>,
    expected: Option<jsonschema::Validator>,
}
```

Functions (exactly these):

```rust
/// Trims; 1–200 characters; must not parse as a UUID. Field `name`.
/// Messages: "must be 1 to 200 characters", "must not be a UUID".
pub(crate) fn check_name(name: &str) -> Result<String, FieldError>;

/// At most 2 000 characters. An empty string is `None`. Field `description`.
/// Message: "must be at most 2000 characters".
pub(crate) fn check_description(text: &str) -> Result<Option<String>, FieldError>;

/// Serializes `schema`, checks its size, compiles it (Draft 2020-12, offline). Returns the text
/// to store. Field `field` (`input_schema` or `expected_schema`).
/// Messages: "must be a JSON object", "must be at most 65536 bytes", "is not a valid JSON Schema".
pub(crate) fn check_schema(field: &str, schema: &Value) -> Result<String, FieldError>;

impl Schemas {
    /// Compiles the stored schema texts. A stored text that no longer parses or compiles is
    /// `Err(column)` (`"input_schema"` or `"expected_schema"`).
    pub(crate) fn compile(input: Option<&str>, expected: Option<&str>)
        -> Result<Self, &'static str>;
    /// [`Self::compile`] of a stored dataset; an `Err(column)` becomes
    /// `MetaError::Corrupt { table: "datasets", column }`. Compile once per request, then call
    /// [`check_item`] per item (M2's import checks up to 50 000 rows with one `Schemas`).
    pub(crate) fn of(dataset: &crate::meta::datasets::DatasetRow) -> Result<Self, crate::meta::MetaError>;
    /// No schema at all.
    pub(crate) fn none() -> Self;
}

/// Every problem with one item, fields prefixed by `prefix` (e.g. `items[3]` gives
/// `items[3].input`; an empty prefix gives `input`). Rules, in order:
/// - `input` is not JSON null — "must not be null";
/// - the serialized size of input + expected_output + metadata ≤ MAX_ITEM_BYTES —
///   field `<prefix>` (or `item` when the prefix is empty), "is larger than 1 MiB";
/// - `input` fits the input schema — "<instance path>: <keyword>" of the first error;
/// - `expected_output`, when present, fits the expected schema — same message form.
pub(crate) fn check_item(prefix: &str, item: &ItemValues, schemas: &Schemas) -> Vec<FieldError>;
```

`impl fmt::Debug for ItemValues` prints `ItemValues { bytes: <serialized size of the three fields>, .. }` (`finish_non_exhaustive`), never a value.

Notes for the implementation:
- `check_name`: `let name = name.trim(); let count = name.chars().count();` refuse `0` or `> MAX_NAME_CHARS`; refuse `name.parse::<Uuid>().is_ok()`.
- `check_schema`: refuse anything but `Value::Object`; `serde_json::to_string(schema)` and its `len()`; compile with `jsonschema::options().with_draft(jsonschema::Draft::Draft202012).offline().build(schema)`, dropping the compiler error (its text can quote the schema). Reuse that one builder in a private `fn compile(schema: &Value) -> Option<jsonschema::Validator>` that `Schemas::compile` also uses.
- Messages from a validator: `let error = validator.iter_errors(value).next()`, message `format!("{}: {}", error.instance_path(), error.kind().keyword())`. An empty instance path renders as an empty string, so the message starts with `": "`; write it as `"/: <keyword>"` in that case so the UI always shows a path.
- `FieldError { field, message, position: None }`.

### 3. `api/datasets.rs`

**Any JSON.** Item inputs, expected outputs and (in M2, M3) task outputs and preview rows are arbitrary JSON: strings, numbers, arrays, objects. `#[schema(value_type = Object)]` would emit `{"type": "object"}` and make the request-example check and `schema.gen.ts` reject a string input. Add one helper to `crates/server/src/api/common.rs` (imports `utoipa::openapi::schema::{ObjectBuilder, Schema, SchemaType}` and `utoipa::openapi::RefOr`, merged with the file's existing `use` lines):

```rust
/// The OpenAPI schema of a field that holds any JSON value (D-J-2): `AnyValue`, as
/// `api::payload`'s `json` branch. Use it with `#[schema(schema_with = any_json)]`.
pub(crate) fn any_json() -> RefOr<Schema> {
    ObjectBuilder::new()
        .schema_type(SchemaType::AnyValue)
        .description(Some("Any JSON value."))
        .into()
}
```

Every input, expected-output, output and preview-row field of J uses `#[schema(schema_with = any_json)]` (on `Value` and `Option<Value>` alike; utoipa still derives `required` from the `Option`). `value_type = Object` stays only for real objects: `metadata` and the two JSON Schemas. If utoipa rejects `schema_with` on an `Option<Option<Value>>` field, keep `schema_with = any_json` and say so; if it rejects `schema_with` entirely, STOP and report.

Module doc: `//! Dataset request and response types (spec 09 § Datasets & experiments, D-J-2 to D-J-5).`

Derives: responses `#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]`; requests add `#[serde(deny_unknown_fields)]`; query params `#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)] #[serde(deny_unknown_fields)]`. Every field has a doc comment (it becomes the OpenAPI description). Optional response fields carry `#[serde(default, skip_serializing_if = "Option::is_none")]`. Times as in `api/score_definitions.rs`. Trace and span ids are `cetra_core::ids::{TraceId, SpanId}` (they serialize as lowercase hex and have `ToSchema`).

```rust
/// A dataset (D-J-2, D-J-3).
pub struct Dataset {
    pub id: Uuid,
    /// Unique in the project, 1 to 200 characters, never a UUID.
    pub name: String,
    pub description: Option<String>,
    /// JSON Schema (Draft 2020-12) every item input must fit. Omitted when unset.
    #[schema(value_type = Option<Object>)]
    pub input_schema: Option<Value>,
    /// JSON Schema every present expected output must fit. Omitted when unset.
    #[schema(value_type = Option<Object>)]
    pub expected_schema: Option<Value>,
    /// Latest version; 0 for a dataset with no change yet.
    pub current_version: i64,
    /// Live items at the latest version.
    pub item_count: i64,
    /// Experiments run on this dataset.
    pub experiment_count: i64,
    pub created_at: i64,   // time_serde
    pub updated_at: i64,   // time_serde
}

/// `POST /api/v1/projects/{project}/datasets` body.
pub struct CreateDatasetRequest {
    pub name: String,
    pub description: Option<String>,
    #[schema(value_type = Option<Object>)]
    pub input_schema: Option<Value>,
    #[schema(value_type = Option<Object>)]
    pub expected_schema: Option<Value>,
}

/// `PATCH /api/v1/projects/{project}/datasets/{dataset}` body. An absent field is unchanged;
/// `null` clears `description` or a schema.
pub struct UpdateDatasetRequest {
    pub name: Option<String>,                 // skip_serializing_if none; null is refused by serde
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub description: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<Object>)]
    pub input_schema: Option<Option<Value>>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<Object>)]
    pub expected_schema: Option<Option<Value>>,
}

/// One version of a dataset.
pub struct DatasetVersion {
    pub version: i64,
    /// What changed, e.g. `add 12 items`.
    pub description: String,
    /// The user id, or `key:<key id>` for an API key. Omitted when unknown.
    pub created_by: Option<String>,
    pub created_at: i64,   // time_serde
}

/// One item as it is in a version.
pub struct DatasetItem {
    /// The item id, stable across versions.
    pub id: Uuid,
    /// This revision.
    pub revision_id: Uuid,
    /// The version that introduced this revision.
    pub version: i64,
    /// Any JSON value.
    #[schema(schema_with = any_json)]
    pub input: Value,
    /// Any JSON value. Omitted when absent.
    #[schema(schema_with = any_json)]
    pub expected_output: Option<Value>,
    #[schema(value_type = Object)]
    pub metadata: Map<String, Value>,
    pub source_trace_id: Option<TraceId>,
    pub source_span_id: Option<SpanId>,
    pub created_at: i64,   // time_serde
}

/// One item to add. `Debug` is hand-written (sizes only), like `ItemValues`.
pub struct NewDatasetItem {
    /// Any JSON value but null.
    #[schema(schema_with = any_json)]
    pub input: Value,
    /// Any JSON value.
    #[schema(schema_with = any_json)]
    pub expected_output: Option<Value>,
    #[schema(value_type = Option<Object>)]
    pub metadata: Option<Map<String, Value>>,
}

/// `POST …/datasets/{dataset}/items` body: 1 to 1 000 items, one new version.
pub struct AddItemsRequest {
    pub items: Vec<NewDatasetItem>,
}

/// The new version and the new item ids, in request order.
pub struct AddItemsResponse {
    pub version: i64,
    pub item_ids: Vec<Uuid>,
}

/// `PATCH …/items/{item_id}` body. Absent keeps; `expected_output: null` clears it.
pub struct UpdateItemRequest {
    #[schema(schema_with = any_json)]
    pub input: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    #[schema(schema_with = any_json)]
    pub expected_output: Option<Option<Value>>,
    #[schema(value_type = Option<Object>)]
    pub metadata: Option<Map<String, Value>>,
}

/// `GET /datasets` query.
pub struct DatasetListParams {
    /// Case-insensitive substring of the name.
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

/// `GET /datasets/{dataset}/versions` query.
pub struct DatasetVersionListParams {
    /// Maximum number of items to return (1–1000, default 50).
    #[serde(default)]
    #[param(minimum = 1, maximum = 1000, default = 50)]
    pub limit: Limit,
    /// Opaque cursor from a previous page.
    #[serde(default)]
    pub cursor: Option<String>,
}

/// `GET /datasets/{dataset}/items` query.
pub struct DatasetItemListParams {
    /// Version to read, 0 to the current version. Default: the current version.
    #[serde(default)]
    pub version: Option<i64>,
    /// Maximum number of items to return (1–1000, default 50).
    #[serde(default)]
    #[param(minimum = 1, maximum = 1000, default = 50)]
    pub limit: Limit,
    /// Opaque cursor from a previous page.
    #[serde(default)]
    pub cursor: Option<String>,
}

/// `GET /datasets/{dataset}/items/{item_id}` query.
pub struct DatasetItemParams {
    #[serde(default)]
    pub version: Option<i64>,
}
```

`present` is a private function, the same as `account.rs`'s `optional_retention` but generic:

```rust
/// Present JSON is `Some(value)`, including null. An absent field stays the default `None`.
fn present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Some(Option::<T>::deserialize(deserializer)?))
}
```

The three list-param structs copy `ScoreDefinitionListParams`' attributes exactly (`#[serde(default)]` and `#[param(…)]` on `limit`), so `limit` stays optional. `NewDatasetItem` gets the same hand-written `Debug` as `ItemValues` (derive everything else; drop `Debug` from its derive list).

Types with an `Option<Option<_>>` field derive `Deserialize` but not `Serialize` (like `UpdateOnlineRuleRequest`); drop `Serialize` from `UpdateDatasetRequest` and `UpdateItemRequest`. `value_type = Object` is used only for `metadata` (a `Map`) and the two JSON Schemas (objects); `api/evaluators.rs` uses it the same way. If `ToSchema` rejects it for `Option<Value>`, use the form `api/evaluators.rs` uses and say so in your report.

### Tests

In `datasets/mod.rs`:
1. `names` — `"  faq  "` → `"faq"`; `""`, 201 × `"a"` and `"01900000-0000-7000-8000-000000000c01"` are refused with the messages above; 200 × `"é"` is accepted (characters, not bytes).
2. `descriptions` — `""` → `None`; 2 000 characters accepted; 2 001 refused.
3. `schemas` — `{"type":"object"}` accepted and returns its text; `[]` → "must be a JSON object"; `{"type": 5}` → "is not a valid JSON Schema"; an object whose serialization is 65 537 bytes → "must be at most 65536 bytes"; `{"$ref": "https://example.com/s.json"}` is refused (offline: no remote reference).
4. `items_against_schemas` — with the `faq` input schema: `{"question":"x"}` passes; `{"q":"x"}` gives one error, field `items[2].input`, message `/: required`; `{"question": 5}` gives `/question: type`; `input: null` gives "must not be null"; an expected output checked against `{"type":"string"}` with `json!(1)` gives `items[2].expected_output` `/: type`; `expected_output: None` is not checked. No message contains the input's values (assert `!message.contains("x")` for the `{"q":"x"}` case).
5. `item_size` — an input string of 1 048 577 bytes gives the size error with field `items[0]`; with an empty prefix the field is `item`.

In `auth/roles.rs`: the two asserts of § 1.

In `datasets/mod.rs`, also `item_values_debug_prints_no_content` (an `ItemValues` with input `"canary-debug"` formats without it).

In `api/datasets.rs`: `any_json_fields_accept_any_value` — `serde_json::from_value::<NewDatasetItem>(json!({"input": "a string", "expected_output": [1, 2]}))` succeeds, and the `NewDatasetItem` schema (`<NewDatasetItem as utoipa::PartialSchema>::schema()` serialized to JSON) has no `"type": "object"` under `properties.input` or `properties.expected_output`.

In `api/datasets.rs`: `update_requests_tell_absent_from_null` — `{"description": null}` → `Some(None)`; `{}` → `None`; `{"input_schema": {"type":"object"}}` → `Some(Some(_))`; `{"bogus": 1}` fails (`deny_unknown_fields`). The same for `UpdateItemRequest.expected_output`.

### Done when

- `jsonschema` is a normal dependency of `cetra-server`; no version changed.
- `api::common::any_json` exists and no J input/expected field uses `value_type = Object`.
- The module, the permission and the types exist as written; `just check` is green.

---

## J1.3 — Dataset routes, OpenAPI, examples, access suite

**Commit subject:** `server: add the dataset routes`

### Read first

- `crates/server/src/routes/score_definitions.rs` (route table, handlers, `bad_request`, audit logs, the read-then-write `write_update` pattern) and `crates/server/src/routes/saved_views.rs` (`DefaultBodyLimit` per route).
- `crates/server/src/routes/orgs.rs`: `page_named`, `field_error`.
- `crates/server/src/auth/principal.rs`: `Principal` (`Session`, `ApiKey`, `Pat`, `ControlPlane`).
- `crates/server/src/http/access.rs`: `routes()` (the `routes.extend` list) and the test that matches routes to the OpenAPI document (around `if !route.documented`).
- `crates/server/src/http/access_suite.rs`: `concrete_path`.
- `crates/server/src/api/openapi.rs`: `tag(…)` list, `collect_schemas`, the `paths = …_paths(paths)` chain, `score_definition_paths`, `operation`, `project_param`, `path_string`, `path_uuid`, `with_query`, `json_of`, `json_named`, `json_body`, `responses`, `QUERY_ERRORS`.
- `crates/server/tests/openapi.rs`: `OPERATIONS`, `QUERY_OR_BODY`, `TAGS`, the error branches in `document_lint`, `pages`.
- `crates/server/src/api_examples.rs` (`generate`, `Output::add`, `fetch`, `example_created`) and `crates/server/tests/api_examples.rs` (`static_files_match_their_types`, `request_examples_match_their_types_and_the_openapi_document`, `generated_files_roundtrip`, `manifest_covers_the_read_operations`).
- `crates/server/src/test_support/mod.rs`: `TestApp`, `seed`, `Seeded`, `seed_examples`, `EXAMPLE_PROJECT`, `insert_key`, `add_member`, `personal_token`.
- `fixtures/api/README.md`.

### Files

| File | Change |
|---|---|
| `crates/server/src/routes/datasets.rs` | new: the five dataset routes |
| `crates/server/src/routes/mod.rs` | `pub(crate) mod datasets;` |
| `crates/server/src/http/access.rs` | `routes.extend(crate::routes::datasets::routes());` right after `score_definitions` |
| `crates/server/src/http/access_suite.rs` | `concrete_path`: `.replace("{dataset}", "faq")` and `.replace("{item_id}", "01900000-0000-7000-8000-000000009999")` before the `{id}` replacement |
| `crates/server/src/datasets/mod.rs` | remove the `allow(dead_code)` line |
| `crates/server/src/api/openapi.rs` | tag `datasets`, schemas, `dataset_paths` |
| `crates/server/tests/openapi.rs` | `OPERATIONS`, `TAGS`, `QUERY_OR_BODY`, error branches, `pages` |
| `crates/server/src/test_support/mod.rs` | `seed_example_faq_dataset`, `write_key` |
| `crates/server/src/api_examples.rs` | the dataset examples |
| `crates/server/tests/api_examples.rs` | round-trips and the request check |
| `fixtures/api/requests/dataset-create.json` | new |
| `fixtures/api/responses/datasets.json`, `fixtures/api/responses/datasets/01900000-0000-7000-8000-000000000c01.json`, `fixtures/api/manifest.json` | generated by `just api-examples` |
| `fixtures/api/README.md` | the dataset rows in § Fixed identifiers |
| `crates/server/openapi.json`, `web/src/api/schema.gen.ts` | generated by `just openapi` |

### 1. Routes

```rust
/// Bodies of dataset create and update: a name, a description and two schemas of ≤ 64 KiB.
const DATASET_BODY_LIMIT: usize = 256 * 1024;
```

| Method | Path | Perm | Handler |
|---|---|---|---|
| GET | `/api/v1/projects/{project}/datasets` | `View` | `list_datasets` |
| POST | `/api/v1/projects/{project}/datasets` | `ManageDatasets` | `create_dataset` (`.layer(DefaultBodyLimit::max(DATASET_BODY_LIMIT))`) |
| GET | `/api/v1/projects/{project}/datasets/{dataset}` | `View` | `get_dataset` |
| PATCH | `/api/v1/projects/{project}/datasets/{dataset}` | `ManageDatasets` | `update_dataset` (same limit) |
| DELETE | `/api/v1/projects/{project}/datasets/{dataset}` | `ManageDatasets` | `delete_dataset` |

Every row: `Access::Project { perm, session_only: false }`, `documented: true`. Handler names inside the module may not clash with the store's imports; import the store as `use crate::meta::datasets as store;` and call `store::list_datasets(…)` etc.

Shared helpers. J1.4 reuses them, and M2 and M3 call them from their own route modules, so they are `pub(crate)` (§ Interfaces for M2 and M3):

```rust
#[derive(Debug, Deserialize)]
pub(crate) struct DatasetPath { pub(crate) dataset: String }

/// Who made a change: a user id, `key:<key id>`, or `None` (the control plane never reaches
/// these routes).
pub(crate) fn created_by(principal: &Principal) -> Option<String> {
    match principal {
        Principal::Session { user, .. } | Principal::Pat { user, .. } => Some(user.id.to_string()),
        Principal::ApiKey { key_id, .. } => Some(format!("key:{key_id}")),
        Principal::ControlPlane { .. } => None,
    }
}

/// The dataset named by the path (`DatasetKey::parse(raw)`, read on the reader pool), or 404.
pub(crate) async fn find(state: &AppState, project: ProjectId, raw: &str) -> Result<DatasetRow, ApiError>;

/// API form of a stored dataset. A stored schema that does not parse is `MetaError::Corrupt`.
pub(crate) fn dataset_json(row: &DatasetRow) -> Result<Dataset, ApiError>;

fn bad_request(errors: Vec<FieldError>) -> ApiError;          // as in score_definitions.rs
```

Audit log on every successful write, with `actor` = `created_by(&principal).as_deref().unwrap_or("-")`:

```rust
tracing::info!(target: "audit", dataset_id = %id, project_id = %project, actor = %actor, "dataset created");
```

(`"dataset updated"`, `"dataset deleted"`.)

**`list_datasets`** (`DatasetListParams`): `store::list_datasets`; keep rows whose `name.to_lowercase()` contains `q.to_lowercase()` when `q` is set (an empty or whitespace `q` is ignored); `page_named(rows.map(|r| (r.name.to_lowercase(), r.id.to_string(), dataset_json(&r)?)), cursor, limit.get())`. The sort key is the lowercased name, matching the store's order.

**`create_dataset`** (`CreateDatasetRequest`): collect field errors from `check_name`, `check_description` (when present), `check_schema("input_schema", …)` and `check_schema("expected_schema", …)` (when present); any → 400 with all of them. Then `store::insert_dataset` with `Uuid::now_v7()` in one `state.meta.write`. Answer `201` with `dataset_json`.

**`get_dataset`**: `find`, `dataset_json`.

**`update_dataset`** (`UpdateDatasetRequest`): validate the fields that are present (a present `name` through `check_name`; `description: Some(Some(text))` through `check_description`, `Some(None)` clears; a present schema through `check_schema`, `Some(None)` clears). Errors → 400. Then one `state.meta.write`:
1. `store::get_dataset_tx(conn, project, &key)` → `None` → return `Ok(Err(UpdateOutcome::NotFound))`.
2. Build the new `DatasetFields` from the stored row and the request.
3. When a schema changes (the new text differs from the stored one) and the new one is `Some`, check the live items: page through `store::live_items_tx(conn, id, after, 1000)` (truncate the extra row and continue from the last `item_id` until a page is short), run `check_item(&format!("items[{}]", row.item_id), …)` against the new schemas, collect up to `MAX_SCHEMA_FAILURES` errors and stop reading when it is reached. Keep a running total of the items' serialized bytes (`input` + `expected_output` + `metadata`, `serde_json::to_string(…).len()`); when it passes `MAX_SCHEMA_CHECK_BYTES` (`64 * 1024 * 1024`, a new `pub(crate) const` in `datasets/mod.rs` with the doc "Item JSON read when a new schema is checked against the live items (D-J-2).") stop and return `Ok(Err(UpdateOutcome::Invalid(vec![FieldError { field: "input_schema" (or "expected_schema", the one that changed; `input_schema` when both did), message: "the dataset is too large to validate against a new schema", position: None }])))`. The single meta writer must not stall on a huge dataset. Any error → `Ok(Err(UpdateOutcome::Invalid(errors)))` (nothing was written).
4. `store::update_dataset(…)` → `Ok(Ok(row))`.

`UpdateOutcome` is a private `enum { NotFound, Invalid(Vec<FieldError>) }`, mapped after the write to `ApiError::NotFound` / `bad_request`. The validation runs in the write transaction so an item added concurrently cannot slip past a new schema (D-J-2).

**`delete_dataset`**: `find` for the id, then `store::delete_dataset` in a write; `false` → 404; answer `204`.

### 2. OpenAPI (`api/openapi.rs`)

- Tag, after `score_definitions`: `tag("datasets", "Versioned datasets and their items.")`.
- `collect_schemas`, after the score definition block: `set.insert::<Dataset>(); set.insert::<CreateDatasetRequest>(); set.insert::<UpdateDatasetRequest>(); set.insert_page::<Dataset>("DatasetPage");` (J1.4 adds the item and version types).
- `fn dataset_paths(paths: PathsBuilder) -> PathsBuilder`, called right after `score_definition_paths` in the chain. A `{dataset}` parameter is `path_string("dataset", "Dataset id, or its name.")`. Operations:

| id | method, path | summary | parameters | body | success | errors |
|---|---|---|---|---|---|---|
| `list_datasets` | GET `{PROJECT}/datasets` | List datasets | `with_query::<DatasetListParams>(vec![project_param()])` | — | `"200"`, `json_named("DatasetPage", "One page of datasets.")` | `QUERY_ERRORS` |
| `post_dataset` | POST `{PROJECT}/datasets` | Create a dataset | `project_param()` | `json_body("CreateDatasetRequest")` | `"201"`, `json_of::<Dataset>("The created dataset.")` | `&[400, 401, 403, 404, 409, 413, 429]` |
| `get_dataset` | GET `{PROJECT}/datasets/{dataset}` | Get a dataset | `project_param()`, dataset | — | `"200"`, `json_of::<Dataset>("The dataset.")` | `&[401, 403, 404, 429]` |
| `patch_dataset` | PATCH `{PROJECT}/datasets/{dataset}` | Update a dataset | same | `json_body("UpdateDatasetRequest")` | `"200"`, `json_of::<Dataset>("The updated dataset.")` | `&[400, 401, 403, 404, 409, 413, 429]` |
| `delete_dataset` | DELETE `{PROJECT}/datasets/{dataset}` | Delete a dataset | same | — | `"204"`, `ResponseBuilder::new().description("Deleted.").build()` | `&[401, 403, 404, 429]` |

Descriptions (`.description(Some(…))`): reads "Any project member, or an API key with `read`."; writes "Project members and above, a read-write personal access token, or an API key with `write` (D-J-5)."; `patch_dataset` adds "Setting a schema validates every live item first; up to 20 failures are listed (D-J-2)."; `delete_dataset` adds "Experiments keep their results; their dataset link is cleared."

### 3. `tests/openapi.rs`

- `TAGS`: add `"datasets"` after `"score_definitions"`.
- `OPERATIONS`: add the five `(method, path, id)` rows (paths with `{project}` and `{dataset}` as in the document).
- `QUERY_OR_BODY`: add `"list_datasets"`.
- Error branches: add `|| *id == "post_dataset" || *id == "patch_dataset"` to a branch expecting `["400", "401", "403", "404", "409", "413", "429"]` — there is none with exactly that set, so add a new `else if` before the `READ_OPERATIONS` branch:

```rust
        } else if *id == "post_dataset" || *id == "patch_dataset" {
            expected_errors.extend(["400", "401", "403", "404", "409", "413", "429"]);
```

  `get_dataset` and `delete_dataset` fall through to the final `["401", "403", "404", "429"]` branch.
- `pages`: add `("DatasetPage", "Dataset")`.

Then `just openapi` and commit both generated files.

### 4. Examples

`test_support/mod.rs`, in `impl TestApp`, after `seed_examples`:

```rust
    /// The example dataset `faq` of `fixtures/api/README.md` § Fixed identifiers, written
    /// through the store into the example project. Call after `seed_examples`.
    /// Not to be confused with `load_example_dataset`, which loads the example spans and
    /// scores of `fixtures/api/dataset/` into storage.
    pub(crate) async fn seed_example_faq_dataset(&self) { … }
```

It writes, in one `self.state.meta.write`, with `now = self.state.clock.now_ns()`: `store::insert_dataset` with id `…0c01` and the fields of § The example dataset; `write_version` v1 with the three revisions (`created_by: None`, description `add 3 items`); `write_version` v2 closing `…0d02` and inserting `…0d14` (description `edit item 01900000-0000-7000-8000-000000000d02`). Use `ProjectId` parsed from `EXAMPLE_PROJECT`. Revision `…0d13`'s source ids are parsed from the hex strings.

Also in `impl TestApp`:

```rust
    /// A `sk-cetra-…` key with `["read", "write"]` on `seeded.project`, as its plaintext.
    pub(crate) async fn write_key(&self, seeded: &Seeded) -> String { … }
```

built like the keys in `seed` (`secret::new_key(KeyKind::Secret)`, `insert_key(conn, project, key, "secret", vec![KeyScope::Read, KeyScope::Write], seeded.user_id, now)`).

`api_examples.rs` `generate()`, right after `load_example_dataset`: `app.seed_example_faq_dataset().await;`. After the scores example:

```rust
    let datasets: Page<Dataset> = fetch(&app, cookie, "/datasets").await;
    out.add("list_datasets", &[], &[], "responses/datasets.json", &datasets);
    let dataset: Dataset = fetch(&app, cookie, &format!("/datasets/{EXAMPLE_DATASET}")).await;
    out.add("get_dataset", &[("dataset", EXAMPLE_DATASET)], &[],
        &format!("responses/datasets/{EXAMPLE_DATASET}.json"), &dataset);
```

with `const EXAMPLE_DATASET: &str = "01900000-0000-7000-8000-000000000c01";` at the top of `api_examples.rs`. Add to the static list: `example_created("post_dataset", BTreeMap::new(), "responses/datasets/01900000-0000-7000-8000-000000000c01.json")` — the create answer has the same shape as the read; reusing the generated file keeps it real.

`fixtures/api/requests/dataset-create.json`:

```json
{
  "name": "faq",
  "description": "Support questions and expected answers",
  "input_schema": {
    "type": "object",
    "required": ["question"],
    "properties": { "question": { "type": "string" } }
  }
}
```

`tests/api_examples.rs`:
- `request_examples_match_their_types_and_the_openapi_document`: `roundtrip::<CreateDatasetRequest>("requests/dataset-create.json");` and the pair `("post_dataset", "requests/dataset-create.json")` in `requests`.
- `generated_files_roundtrip`: `"list_datasets" => roundtrip::<Page<Dataset>>(&example.file),` and `"get_dataset" | "post_dataset" => roundtrip::<Dataset>(&example.file),`.

Run `just api-examples`, then check `fixtures/api/responses/datasets.json` holds one dataset `faq` with `current_version: 2`, `item_count: 3`, `experiment_count: 0`. `fixtures/api/README.md` § Fixed identifiers gains rows for the dataset, the items and the revisions (the table of § Fixed identifiers above).

### Tests (`routes/datasets.rs`, `#[cfg(test)] mod tests`)

All tests but `audit_log_names_the_key` are `#[tokio::test(flavor = "multi_thread")]`. Setup: `let app = TestApp::new().await; let seeded = app.seed().await;` and `let base = format!("/api/v1/projects/{}/datasets", seeded.project.id);`. Post with `.header("cookie", &seeded.cookie).json(&json!({…}))`.

1. `create_get_list_delete` — POST `{"name":"faq","input_schema":{"type":"object"}}` → 201, `current_version` 0, `item_count` 0, `input_schema` echoed; GET by id and GET `/datasets/faq` both 200 and equal; list → one row; DELETE → 204; GET → 404.
2. `create_validates` — `{"name":"01900000-0000-7000-8000-000000000001"}` → 400 field `name` "must not be a UUID"; `{"name":"x","input_schema":{"type":5}}` → 400 field `input_schema`; `{"name":"x","extra":1}` → 400 (unknown field); a second `faq` → 409 with detail `a dataset with this name already exists`.
3. `list_filters_and_pages` — create `Beta`, `alpha`, `gamma`; `?q=ET` → only `Beta` (case-insensitive); `?q=%20` → all three (whitespace is ignored); `?limit=2` → `alpha`, `Beta` and a `next_cursor`; the cursor page → `gamma`, `next_cursor` null.
4. `patch_fields_and_clears` — PATCH `{"description":"d"}` sets it; `{"description":null}` clears it; `{"name":"renamed"}` renames (GET `/datasets/renamed` works); `{}` changes nothing but `updated_at`.
5. `schema_change_checks_live_items` — create with no schema; insert two live items directly through `store::write_version` (inputs `{"question":"a"}` and `{"q":"b"}`) in `app.state.meta.write`; PATCH `{"input_schema": <faq input schema>}` → 400 with exactly one error whose field is `items[<id of the second>].input` and message `/: required`; the dataset still has no schema; after a `write_version` that closes the second item, the same PATCH → 200.
6. `schema_check_is_capped` — `#[tokio::test(flavor = "multi_thread")]`: insert through `store::write_version` 70 live items whose input is `{"question": "<1 MiB minus 100 bytes of 'a'>"}` (≈ 70 MiB, under the per-item cap); PATCH a schema `{"type":"object","required":["question"]}` (which every item fits) → 400 with one error, field `input_schema`, message `the dataset is too large to validate against a new schema`; the dataset still has no schema.
7. `roles` — a viewer (`let viewer = app.add_member(&seeded, "v@example.com", OrgRole::Viewer).await;`, then `viewer.cookie`) gets 200 on GET and 403 on POST; the seeded member (`seeded.cookie`) gets 201.
8. `keys` — `seeded.read_key` (`Authorization: Bearer …`) gets 200 on GET and 403 on POST; `app.write_key(&seeded)` gets 201 on POST.
9. `personal_tokens` — `app.personal_token(seeded.user_id, true)` (read-write) gets 201 on POST; `app.personal_token(seeded.user_id, false)` (read-only) gets 200 on GET and 403 on POST.
10. `audit_log_names_the_key` — a separate `#[tokio::test]` (current thread: `capture_logs` needs it; never mix it with the `multi_thread` tests above): `let (response, logs) = crate::test_support::capture_logs(async { … }).await;` around a POST with `app.write_key(&seeded)`; 201; `logs` contains `actor=key:` and the new dataset's id, and does not contain the dataset name.
11. `other_projects_datasets_are_404` — a dataset of a second project (`app.add_project`) is 404 by id through the first project's path.

### Done when

- The five routes answer as above; OpenAPI, the access suite, the examples and the manifest are updated and generated; `just check` is green.

---

## J1.4 — Items and versions; closeout

**Commit subject:** `server: add dataset items and versions`

### Read first

- J1.3's `routes/datasets.rs`, the store and `datasets/mod.rs`.
- `crates/server/src/api_examples.rs` as J1.3 left it.

### Files

| File | Change |
|---|---|
| `crates/server/src/routes/datasets.rs` | the six item and version routes |
| `crates/server/src/api/openapi.rs` | schemas and operations |
| `crates/server/tests/openapi.rs` | lists, branches, `pages` |
| `crates/server/src/api_examples.rs`, `crates/server/tests/api_examples.rs` | examples and round-trips |
| `fixtures/api/requests/dataset-items-add.json` | new |
| `fixtures/api/responses/datasets/…`, `fixtures/api/manifest.json` | generated |
| `crates/server/openapi.json`, `web/src/api/schema.gen.ts` | generated |
| `docs/spec/09-api.md` | only if the routes differ from the J plan's amendment (they must not; STOP if they would) |
| `CHANGELOG.md` | one `### Added` line |
| `docs/roadmap.md` | J's row: status **In progress** |

### 1. Routes

```rust
/// A batch add: up to 1 000 items of at most 1 MiB, within 16 MiB (D-J-4).
const ITEMS_BODY_LIMIT: usize = 16 * 1024 * 1024;
/// One item edit: an item of at most 1 MiB plus JSON overhead.
const ITEM_BODY_LIMIT: usize = 2 * 1024 * 1024;
```

| Method | Path | Perm | Handler |
|---|---|---|---|
| GET | `…/datasets/{dataset}/versions` | `View` | `list_versions` |
| GET | `…/datasets/{dataset}/items` | `View` | `list_items` |
| POST | `…/datasets/{dataset}/items` | `ManageDatasets` | `add_items` (`ITEMS_BODY_LIMIT`) |
| GET | `…/datasets/{dataset}/items/{item_id}` | `View` | `get_item` |
| PATCH | `…/datasets/{dataset}/items/{item_id}` | `ManageDatasets` | `update_item` (`ITEM_BODY_LIMIT`) |
| DELETE | `…/datasets/{dataset}/items/{item_id}` | `ManageDatasets` | `delete_item` |

`…` is `/api/v1/projects/{project}`. Path struct `ItemPath { dataset: String, item_id: Uuid }` (a malformed `item_id` is the extractor's 400).

Helpers:

```rust
/// `?version=`: 0 to the dataset's current version, default the current version.
/// Message on field `version`: "must be between 0 and <current_version>".
pub(crate) fn version_of(row: &DatasetRow, requested: Option<i64>) -> Result<i64, ApiError>;

/// API form of a revision.
pub(crate) fn item_json(row: &ItemRow) -> DatasetItem;   // id = item_id, version = valid_from_version

/// "add 1 item" / "add <n> items".
fn add_description(count: usize) -> String;
```

**`list_versions`**: cursor = the decimal text of the last version on the page (an unparseable cursor → 400 field `cursor` "invalid cursor"); `store::list_versions(pool, id, before, limit)`; when it returns `limit + 1` rows, drop the last and set `next_cursor` to the last kept version.

**`list_items`**: `version_of`; cursor = the last item id on the page (not a UUID → 400 field `cursor` "invalid cursor"); `store::list_items`; the same `limit + 1` rule.

**`add_items`** (`AddItemsRequest`): 1 to `MAX_BATCH_ITEMS` items, else 400 field `items` "must hold 1 to 1000 items". Then one write:
1. `get_dataset_tx` (`None` → `NotFound`); `Schemas::compile` from its stored texts (an `Err(column)` → `MetaError::Corrupt { table: "datasets", column }`).
2. For each item `i`: `ItemValues { input, expected_output, metadata: metadata.unwrap_or_default() }`, `check_item(&format!("items[{i}]"), …)`. Any error → `Ok(Err(Invalid(all errors)))`, nothing written.
3. A `VersionChange` with no close, one `NewRevision` per item (`item_id` and `revision_id` = `Uuid::now_v7()`, no source), `description = add_description(n)`, `created_by`. `store::write_version`.
4. `Ok(Ok(AddItemsResponse { version, item_ids }))`.

Answer `201`. `TOO_MANY_ITEMS` surfaces as 409 through `?`. Audit: `"dataset items added"` with `dataset_id`, `count`, `version`.

**`get_item`** (`DatasetItemParams`): `version_of`, `store::get_item` → `None` → 404.

**`update_item`** (`UpdateItemRequest`): one write:
1. `get_dataset_tx` → 404; `get_live_item_tx(dataset, item)` → `None` → 404 (an item deleted at the latest version cannot be edited).
2. Merge: `input` = body's or the live one; `expected_output` = `Some(Some(v))` → `Some(v)`, `Some(None)` → `None`, absent → live; `metadata` = body's or live.
3. When the merged values equal the live revision's (`Value` equality; with `preserve_order` an object compares as a map), return `Ok(Ok(live))`: no version (D-J-3).
4. `check_item("", …)` against the dataset's schemas → errors → `Invalid`.
5. `write_version` with `close: vec![item]`, one `NewRevision` with the same `item_id`, a new `revision_id`, the merged values and the live revision's `source_trace_id` / `source_span_id` (edits keep the source, D-J-9), description `format!("edit item {item}")`.
6. Read the new revision with `get_live_item_tx` and return it.

**`delete_item`**: one write: `get_dataset_tx` → 404; `get_live_item_tx` → 404; `write_version` with `close: vec![item]`, no insert, description `format!("delete item {item}")`; answer `200` with the new `DatasetVersion` (read with `store::list_versions(…, None, 1)` after the write, or build it from the values written: `version`, description, `created_by`, `created_at = now`; build it, no second read).

### 2. OpenAPI

Schemas: `DatasetVersion`, `DatasetItem`, `NewDatasetItem`, `AddItemsRequest`, `AddItemsResponse`, `UpdateItemRequest`, and `set.insert_page::<DatasetVersion>("DatasetVersionPage"); set.insert_page::<DatasetItem>("DatasetItemPage");`. An `{item_id}` parameter is `path_uuid("item_id", "Item id, stable across versions.")`.

| id | method, path | summary | parameters | body | success | errors |
|---|---|---|---|---|---|---|
| `list_dataset_versions` | GET `…/datasets/{dataset}/versions` | List the versions of a dataset | `with_query::<DatasetVersionListParams>(vec![project, dataset])` | — | `"200"`, `json_named("DatasetVersionPage", …)` | `QUERY_ERRORS` |
| `list_dataset_items` | GET `…/datasets/{dataset}/items` | List the items of a version | `with_query::<DatasetItemListParams>(…)` | — | `"200"`, `json_named("DatasetItemPage", …)` | `QUERY_ERRORS` |
| `post_dataset_items` | POST `…/datasets/{dataset}/items` | Add items | project, dataset | `json_body("AddItemsRequest")` | `"201"`, `json_of::<AddItemsResponse>(…)` | `&[400, 401, 403, 404, 409, 413, 429]` |
| `get_dataset_item` | GET `…/items/{item_id}` | Get an item | `with_query::<DatasetItemParams>(vec![project, dataset, item])` | — | `"200"`, `json_of::<DatasetItem>(…)` | `QUERY_ERRORS` |
| `patch_dataset_item` | PATCH `…/items/{item_id}` | Edit an item | project, dataset, item | `json_body("UpdateItemRequest")` | `"200"`, `json_of::<DatasetItem>(…)` | `&[400, 401, 403, 404, 409, 413, 429]` |
| `delete_dataset_item` | DELETE `…/items/{item_id}` | Delete an item | project, dataset, item | — | `"200"`, `json_of::<DatasetVersion>(…)` | `&[400, 401, 403, 404, 409, 429]` |

Descriptions say which version a write creates ("Creates one new version."), that a no-op edit creates none, and the permission sentence of J1.3.

`tests/openapi.rs`: the six `OPERATIONS` rows; `QUERY_OR_BODY` gets `"list_dataset_versions"`, `"list_dataset_items"`, `"get_dataset_item"`; extend J1.3's new branch with `post_dataset_items` and `patch_dataset_item`; add

```rust
        } else if *id == "delete_dataset_item" {
            expected_errors.extend(["400", "401", "403", "404", "409", "429"]);
```

`pages`: `("DatasetVersionPage", "DatasetVersion")`, `("DatasetItemPage", "DatasetItem")`. Then `just openapi`.

### 3. Examples

In `generate()`, after the J1.3 dataset examples:

| operation | path params | query | file |
|---|---|---|---|
| `list_dataset_versions` | `dataset` | — | `responses/datasets/<id>/versions.json` |
| `list_dataset_items` | `dataset` | — | `responses/datasets/<id>/items.json` (version 2) |
| `list_dataset_items` | `dataset` | `version=1` | `responses/datasets/<id>/items-v1.json` |
| `get_dataset_item` | `dataset`, `item_id` = `…0d02` | — | `responses/datasets/<id>/items/01900000-0000-7000-8000-000000000d02.json` |

each through `fetch` and `out.add`, `<id>` = `EXAMPLE_DATASET`. `fixtures/api/requests/dataset-items-add.json`:

```json
{
  "items": [
    {
      "input": { "question": "Do you support Gemini?" },
      "expected_output": { "answer": "Yes, through the OpenAI-compatible or Gemini adapters." },
      "metadata": { "topic": "integrations" }
    },
    {
      "input": "What is the free plan's data window?",
      "expected_output": "30 days"
    }
  ]
}
```

The second item's string input and output check that any-JSON fields pass `request_examples_match_their_types_and_the_openapi_document` (blocker of the plan review: a `value_type = Object` schema would fail here).

`tests/api_examples.rs`: `roundtrip::<AddItemsRequest>("requests/dataset-items-add.json")`, the pair `("post_dataset_items", "requests/dataset-items-add.json")`, and in `generated_files_roundtrip` `"list_dataset_versions" => roundtrip::<Page<DatasetVersion>>(…)`, `"list_dataset_items" => roundtrip::<Page<DatasetItem>>(…)`, `"get_dataset_item" => roundtrip::<DatasetItem>(…)`. Run `just api-examples`; `items.json` must list `…0d01`, `…0d02` (revision `…0d14`) and `…0d03`; `items-v1.json` the three v1 revisions.

### 4. Changelog and roadmap

`CHANGELOG.md` § `[Unreleased]` → `### Added`, one line:

```
- Datasets: versioned datasets of input / expected output / metadata items with optional JSON Schemas, and their API (`/datasets`, items, versions) (track J).
```

`docs/roadmap.md`, J's row: Plan column `[plans/j-datasets/](plans/j-datasets/): design doc, then one plan per milestone (M1–M6)`; Status **In progress.** "M1 (datasets, items, versions) implemented." Decisions column `D-J-1–D-J-19`.

### Tests (`routes/datasets.rs`)

1. `add_list_and_version` — create `faq` with the example input schema; POST two items → 201, `version` 1, two ids in order; GET `/items` → both, each `version` 1; GET `/versions` → one row `add 2 items` with `created_by` = the user id; GET `/datasets/faq` → `current_version` 1, `item_count` 2.
2. `add_validates_every_item` — a batch where item 0 is fine and item 1 is `{"input":{"q":1}}` → 400 with one error field `items[1].input`; no version was created (`current_version` still 0); `{"items":[]}` → 400 field `items`; 1 001 items → 400.
3. `edit_creates_a_version_and_keeps_history` — add one item; PATCH `{"expected_output":{"a":1}}` → 200, `version` 2, same `id`, new `revision_id`; GET `?version=1` of the item has no `expected_output`; PATCH with the same body again → 200 and `current_version` stays 2; PATCH `{"expected_output":null}` → version 3 without it; PATCH `{"input":{"q":1}}` against the schema → 400 field `input`.
4. `delete_item` — DELETE → 200 with `version` 2 and description `delete item <id>`; GET item → 404; GET `?version=1` of it → 200; second DELETE → 404; PATCH → 404.
5. `version_bounds_and_cursors` — `?version=5` on a dataset at version 1 → 400 field `version`; `?version=-1` → 400; `?cursor=nope` → 400; three items with `?limit=2` give two items and a cursor, and the cursor page gives the third with `next_cursor` null; after a second version, `/versions?limit=1` gives version 2 with `next_cursor` `"2"`, and that cursor's page gives version 1 with `next_cursor` null.
6. `live_item_cap` — set `store::LIVE_ITEM_CAP` live items through one `write_version` in `app.state.meta.write` (inputs `json!({"question": i.to_string()})`, no schema on that dataset), then POST one item → 409 `a dataset holds at most 50000 items`.
7. `source_ids_survive_an_edit` — insert one revision with source ids through the store; PATCH its metadata over HTTP; the response keeps `source_trace_id` and `source_span_id`.
8. `size_limits` — one item whose input is a string of 1 048 577 bytes → 400 field `items[0]` "is larger than 1 MiB"; a body of 17 MiB (20 items of about 870 KiB each, each under the per-item cap) → 413; a PATCH body of 3 MiB → 413.
9. `personal_tokens_write_items` — a read-write PAT (`app.personal_token(seeded.user_id, true)`) adds an item (201) and its version's `created_by` is the user id; a read-only PAT gets 403.
10. `keys` — `app.write_key(&seeded)` adds items (201) and its version's `created_by` is `key:<uuid>`; `seeded.read_key` lists items (200) and is refused on POST (403).
11. `logs_hold_no_content` — a `#[tokio::test]` (current thread) with `capture_logs`, add an item whose input holds `canary-j1-input`; the captured logs do not contain `canary-j1-input`.

### Done when

- The six routes answer as above; OpenAPI and the examples are regenerated; the changelog and roadmap lines are in; `just check` is green.
- Report: what was done, any deviation (and why), anything left unverified. No new dependency version (J1.2 only moved `jsonschema`).
