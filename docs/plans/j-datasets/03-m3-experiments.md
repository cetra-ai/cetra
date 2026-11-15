# J · M3 — Experiments

Implementation plan for milestone M3 of track J. It has 7 sub-phases, **J3.1 to J3.7**, one commit each, done in order by one agent working alone. **Haiku** implements each sub-phase with no context beyond this file, [`00-design.md`](00-design.md) and the files a sub-phase names; **Sonnet** reviews the milestone. If this plan and the code disagree (a name moved, a function changed), STOP and report instead of guessing. Every sub-phase ends with `just check` green.

M3 adds the server half of experiments (D18): the experiment store, the experiment routes, experiment scores, the two storage reads behind results and comparison, the plan window on experiments, the `dataset_items` and `experiment_items` SQL views, and the whole deletion cascade of D-J-9. M4 (SDKs) and M6 (web) build on its API contract.

## Prerequisites

- M1 is merged on `main`: the migration `NNNN_datasets.sql` (every J table, D-J-19), `crates/server/src/meta/datasets.rs`, `crates/server/src/api/datasets.rs`, `crates/server/src/datasets/mod.rs`, `crates/server/src/routes/datasets.rs` and `ProjectPerm::ManageDatasets` exist. Check:

```bash
grep -n "CREATE TABLE experiment_items" crates/server/migrations/meta/*_datasets.sql
```

```bash
grep -n "ManageDatasets" crates/server/src/auth/roles.rs
```

Each must print at least one line. Otherwise STOP and report.

- M2 may or may not be merged; M3 does not need it (§ Parallel work in the design). If M2 is merged, rebase first and keep its lines in the shared files.
- Read [`01-m1-datasets.md`](01-m1-datasets.md) § Interfaces for M2 and M3. M3 calls these M1 names; check each exists with `grep -n "fn <name>"` before you start, and STOP if one is missing:
  - `routes::datasets::find(state, project, raw) -> Result<DatasetRow, ApiError>`: the dataset named by an id or a name, or 404 (D-J-4);
  - `routes::datasets::created_by(&Principal) -> Option<String>`: a user id or `key:<key id>`;
  - `routes::datasets::item_json(&ItemRow) -> DatasetItem`;
  - `meta::datasets::list_items(pool, dataset, version, after, limit) -> Result<Vec<ItemRow>, MetaError>`: a version's revisions by `item_id`, at most `limit + 1` rows;
  - `meta::datasets::{DatasetRow, ItemRow}`, `TestApp::write_key`, `Seeded::read_key`, `TestApp::seed_example_dataset`.

## Read first, for every sub-phase

- `AGENTS.md`, all of it.
- [`00-design.md`](00-design.md): § Decisions D-J-9 to D-J-15 and D-J-18, § Data model, § API contract, § Parallel work, § Conventions.
- This file up to § J3.1, then your sub-phase.
- `docs/spec/06-evaluation.md` § Experiments, `docs/spec/09-api.md` § Datasets & experiments, `docs/spec/05-query.md` § Logical views (all as amended by the J plan).

## Conventions for every sub-phase

- Time is `i64` nanoseconds; JSON times are RFC 3339 with 9 digits (`llmobs_core::time_serde`, optional ones `crate::api::keys::optional_time`), as in `crates/server/src/api/score_definitions.rs`.
- IDs are `Uuid::now_v7()`, stored as `TEXT` (`id.to_string()`); trace ids are stored as their 16 raw bytes (`trace.as_bytes().to_vec()`), read back with `TraceId::from_slice`, and are lowercase hex in JSON (`TraceId` serializes so).
- Errors are `ApiError`; field errors use `FieldError { field, message, position: None }` with JSON paths (`results[3].repetition`).
- Logs: ids, counts and sizes only. Never an experiment name, a description, metadata, an output, an error text or an item's content.
- No `unwrap`/`expect` outside tests unless a comment states the invariant.
- Rust tests sit in `#[cfg(test)] mod tests` of the module they test. HTTP tests use `TestApp::new().await` and `app.seed().await` (`crate::test_support`), `#[tokio::test(flavor = "multi_thread")]`, except tests that capture logs (`#[tokio::test]`).
- When a sub-phase changes the API contract: `just openapi`, and commit `crates/server/openapi.json` and `web/src/api/schema.gen.ts`. When it changes `fixtures/api/`: `just api-examples`, and commit what it writes.

```bash
export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH
```

## Final layout after M3

```
crates/server/src/meta/experiments.rs        store: experiments, results, the cascade deletes   (J3.1, J3.7)
crates/server/src/api/experiments.rs         API types                                          (J3.4, J3.5)
crates/server/src/api/scores.rs              ScoreCreate + experiment_id, dataset_item_id        (J3.2)
crates/server/src/scores.rs                  experiment checks and fields on write              (J3.2)
crates/storage/src/reads/experiments.rs      experiment_scores, experiment_traces               (J3.3)
crates/server/src/experiments/mod.rs         visible_from (J3.4); summary, results rows and comparison (pure) (J3.5)
crates/server/src/routes/experiments.rs      the routes                                         (J3.4, J3.5)
crates/server/src/datasets/sql_views.rs      the two SQL views and their catalog                (J3.6)
crates/server/src/deletion/resolve.rs        + the J cascade                                    (J3.7)
```

## Parallel work

M2 may run at the same time in another worktree, and S, T, N's M3 and R run alongside J (design § Parallel work). The files M3 shares with them are additive; after a rebase, regenerate instead of hand-merging:

| File | Sub-phase | Also touched by | Rule |
|---|---|---|---|
| `crates/server/src/api/openapi.rs`, `crates/server/openapi.json`, `web/src/api/schema.gen.ts` | J3.2, J3.4, J3.5 | M2, S, T, N M3 | `just openapi` after a rebase |
| `crates/server/tests/openapi.rs`, `http/access.rs`, `api/mod.rs`, `routes/mod.rs`, `api_examples.rs`, `tests/api_examples.rs` | J3.4, J3.5 | M2, S, T | additive lines, alphabetical |
| `fixtures/api/manifest.json`, `fixtures/api/static/schema.json` | J3.4–J3.6 | M2, T | `just api-examples` after a rebase |
| `crates/storage/src/reads/mod.rs`, `crates/storage/src/lib.rs` (`pub use reads::{…}`) | J3.3 | R | additive lines |
| `crates/storage/src/views/catalog.rs` (the `catalog_matches_the_schema_fixture` test) | J3.6 | R | one test line |
| `crates/server/src/deletion/resolve.rs`, `routes/sql.rs`, `mcp/sql.rs` | J3.6, J3.7 | — | only M3 |
| `CHANGELOG.md`, `docs/roadmap.md` | J3.7 | everyone | additive lines |

## Shared reference

### Fixed identifiers

| Id | Use |
|---|---|
| `01900000-0000-7000-8000-000000000c01` | the example dataset `faq` (M1) |
| `01900000-0000-7000-8000-000000000d01`, `…0d02`, `…0d03` | its items (M1) |
| `01900000-0000-7000-8000-000000000d11`, `…0d14`, `…0d13` | their revisions at version 2, the current version of `faq` after M1 (`…0d12` is `…0d02`'s version-1 revision) |
| `01900000-0000-7000-8000-000000000e01` | the example experiment `baseline` |
| `01900000-0000-7000-8000-000000000e02` | the example experiment `candidate` |

The experiment fixtures pin `faq` at version 2.

### Limits (constants in `crates/server/src/api/experiments.rs`)

```rust
/// Repetitions of an experiment (D-J-10).
pub const MAX_REPETITIONS: i64 = 20;
/// Live items × repetitions of an experiment (D-J-10, CG-J2).
pub const MAX_EXPERIMENT_ROWS: i64 = 20_000;
/// Results in one `POST …/items` (D-J-10).
pub const MAX_RESULTS_PER_REQUEST: usize = 500;
/// Serialized size of one result's `output` (D-J-10).
pub const MAX_OUTPUT_BYTES: usize = 256 * 1024;
/// Serialized size of all the outputs of one `POST …/items` (D-J-10).
pub const MAX_OUTPUTS_PER_REQUEST: usize = 32 * 1024 * 1024;
/// Body limit of `POST …/items`: the outputs' cap plus room for the rest (D-J-10).
pub const RESULTS_BODY_LIMIT: usize = 40 * 1024 * 1024;
/// Characters of one result's `error`, and of `finish`'s `error` (D-J-10).
pub const MAX_ERROR_CHARS: usize = 4_096;
/// Serialized size of an experiment's `metadata` (D-J-10).
pub const MAX_METADATA_BYTES: usize = 64 * 1024;
/// Characters of a name (D-J-10).
pub const MAX_NAME_CHARS: usize = 200;
/// Characters of a description (D-J-4's limit, reused).
pub const MAX_DESCRIPTION_CHARS: usize = 2_000;
/// Items in `CreateExperimentResponse` (the first page of the pinned version).
pub const CREATE_ITEMS_PAGE: u32 = 1_000;
/// How far before an experiment's `created_at` its traces and scores are read (client clocks
/// stamp span start times and UUIDv7 score ids), in nanoseconds: one hour.
pub const CLOCK_SKEW_NS: i64 = 3_600_000_000_000;
```

### Status

`running` → `completed` (finish without `error`) or `failed` (finish with `error`). Nothing else changes a status. A second finish whose outcome (`completed` or `failed`) equals the stored status answers the experiment unchanged; another outcome is 409 (D-J-10).

### Visibility (D-J-14)

One helper, created by J3.4 in the new module `crates/server/src/experiments/mod.rs` (J3.5 adds the aggregation to the same module), and used by `routes/experiments.rs` and `datasets/sql_views.rs` (J3.6):

```rust
/// The org's window bound now (`created_at` before it is hidden), or `None` without a window
/// (D-J-14).
pub(crate) fn visible_from(state: &AppState, org: OrgId) -> Option<i64> {
    Visibility::window(state, org).visible_from()
}
```

`Visibility` is `crate::entitlements::Visibility` (re-exported from `entitlements/window.rs`; check the `use` in `deletion/resolve.rs`). An experiment row with `created_at < visible_from` is treated as absent by every route (404) except `DELETE` (deleting reveals nothing, and the rows hold outputs), and by the list, compare and SQL view. This adds no `QueryOptions` and no direct `storage.query(` call, so the D-P-44 source tests in `entitlements/window.rs` stay green.

---

## J3.1 — The experiment store

**Commit subject:** `server: store experiments and their results`

### Files

| File | Change |
|---|---|
| `crates/server/src/meta/experiments.rs` | new |
| `crates/server/src/meta/mod.rs` | `pub(crate) mod experiments;` (alphabetical in the module list) |

No migration: M1 created the tables (§ Data model of the design).

### Read first

- `crates/server/src/meta/mod.rs`: `MetaDb::write`, `MetaDb::reader`, `MetaError` (`Conflict`, `Corrupt`), `unique_or`, the test helper that opens a test database (the one `meta/score_definitions.rs` tests use).
- `crates/server/src/meta/score_definitions.rs` and `crates/server/src/meta/saved_views.rs`: the house style (a `COLUMNS` const, a private `sqlx::FromRow` record, a decoder returning `MetaError::Corrupt`, `_tx` functions on `&mut SqliteConnection`, `sqlx::QueryBuilder` for `IN (…)` lists), and how their tests create an org and a project.
- `crates/server/src/meta/datasets.rs` (M1): its row types and its test helpers that create a dataset with items, which the tests below reuse.
- The migration `crates/server/migrations/meta/NNNN_datasets.sql`.

### Module doc and imports

```rust
//! `experiments` and `experiment_items` rows (spec 04 § Datasets & experiments, D-J-10).
```

Add right after it, as T2 and G's first phases did:

```rust
// Used by tests only until J3.4 serves the rows; J3.4 removes this line.
#![cfg_attr(not(test), allow(dead_code))]
```

### Types (exactly these)

```rust
/// One stored experiment.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ExperimentRow {
    pub(crate) id: Uuid,
    pub(crate) project_id: ProjectId,
    /// `None` once the dataset is deleted (`ON DELETE SET NULL`).
    pub(crate) dataset_id: Option<Uuid>,
    pub(crate) dataset_name: String,
    pub(crate) dataset_version: i64,
    pub(crate) name: String,
    pub(crate) description: Option<String>,
    /// JSON object text.
    pub(crate) metadata: String,
    pub(crate) repetitions: i64,
    pub(crate) item_count: i64,
    /// Rows in `experiment_items` (a subquery count).
    pub(crate) result_count: i64,
    /// `running`, `completed` or `failed`.
    pub(crate) status: String,
    pub(crate) error: Option<String>,
    pub(crate) created_by: Option<String>,
    pub(crate) created_at: i64,
    pub(crate) finished_at: Option<i64>,
}

/// A new experiment, before insert.
#[derive(Debug, Clone)]
pub(crate) struct NewExperiment {
    pub(crate) id: Uuid,
    pub(crate) project_id: ProjectId,
    pub(crate) dataset_id: Uuid,
    pub(crate) dataset_name: String,
    pub(crate) dataset_version: i64,
    pub(crate) name: String,
    pub(crate) description: Option<String>,
    pub(crate) metadata: String,
    pub(crate) repetitions: i64,
    pub(crate) item_count: i64,
    pub(crate) created_by: Option<String>,
    pub(crate) created_at: i64,
}

/// One result to store. `input`, `expected_output` and `revision_id` come from the pinned
/// revision, never from the client (D-J-10).
#[derive(Clone, PartialEq)]
pub(crate) struct NewResult {
    pub(crate) dataset_item_id: Uuid,
    pub(crate) repetition: i64,
    pub(crate) revision_id: Uuid,
    /// JSON text.
    pub(crate) input: String,
    /// JSON text.
    pub(crate) expected_output: Option<String>,
    pub(crate) trace_id: Option<TraceId>,
    /// JSON text.
    pub(crate) output: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) latency_ns: Option<i64>,
}

/// One stored result row.
#[derive(Clone, PartialEq)]
pub(crate) struct ResultRow {
    pub(crate) dataset_item_id: Uuid,
    pub(crate) repetition: i64,
    pub(crate) revision_id: Uuid,
    pub(crate) input: String,
    pub(crate) expected_output: Option<String>,
    pub(crate) trace_id: Option<TraceId>,
    pub(crate) output: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) latency_ns: Option<i64>,
    pub(crate) created_at: i64,
}

/// The light projection of one result: what the aggregation needs, without the text columns
/// (D-J-13). 20,000 of these stay small.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResultKey {
    pub(crate) dataset_item_id: Uuid,
    pub(crate) repetition: i64,
    pub(crate) trace_id: Option<TraceId>,
    /// `error IS NOT NULL`.
    pub(crate) has_error: bool,
    pub(crate) latency_ns: Option<i64>,
}

/// One row of the `experiment_items` SQL view (J3.6).
#[derive(Clone, PartialEq)]
pub(crate) struct ViewRow {
    pub(crate) experiment_id: Uuid,
    pub(crate) experiment_name: String,
    pub(crate) dataset_id: Option<Uuid>,
    pub(crate) dataset_version: i64,
    pub(crate) dataset_item_id: Uuid,
    pub(crate) repetition: i64,
    pub(crate) trace_id: Option<TraceId>,
    pub(crate) input: String,
    pub(crate) expected_output: Option<String>,
    pub(crate) output: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) latency_ns: Option<i64>,
    pub(crate) created_at: i64,
}

/// The pinned revision of one item, for `NewResult`.
#[derive(Clone, PartialEq)]
pub(crate) struct PinnedItem {
    pub(crate) revision_id: Uuid,
    pub(crate) input: String,
    pub(crate) expected_output: Option<String>,
}

/// What the cascade of a deletion removed (D-J-9), for the audit log counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Cascade {
    pub(crate) dataset_item_revisions: u64,
    pub(crate) experiment_items: u64,
}
```

**`Debug` by hand** for `NewResult`, `ResultRow`, `PinnedItem` and `ViewRow` (design § Conventions, Privacy): they hold item content, outputs and error texts, so their `Debug` prints only the ids, the repetition, the trace id, `latency_ns` and the byte length of each text field (`f.debug_struct("ResultRow").field("dataset_item_id", &self.dataset_item_id).field("repetition", &self.repetition).field("trace_id", &self.trace_id).field("input_len", &self.input.len())…finish()`). `ExperimentRow` and `NewExperiment` keep the derived `Debug`: their names and metadata are user text but never item content; still, never log them.

Private `#[derive(sqlx::FromRow)] struct ExperimentRecord` and `ResultRecord` mirror the columns with raw types (`String`, `Option<String>`, `i64`, `Option<i64>`, `Option<Vec<u8>>` for `trace_id`). Decoders `experiment_from(ExperimentRecord) -> Result<ExperimentRow, MetaError>` and `result_from(ResultRecord) -> Result<ResultRow, MetaError>` parse uuids (`Uuid::parse_str`), `ProjectId` (as `score_definitions.rs` does) and `TraceId::from_slice`; a failure is `MetaError::Corrupt { table: "experiments" | "experiment_items", column: "<column>" }`.

Column lists (one `const` each):

```rust
const EXPERIMENT_COLUMNS: &str = "e.id, e.project_id, e.dataset_id, e.dataset_name, \
    e.dataset_version, e.name, e.description, e.metadata, e.repetitions, e.item_count, \
    (SELECT count(*) FROM experiment_items i WHERE i.experiment_id = e.id) AS result_count, \
    e.status, e.error, e.created_by, e.created_at, e.finished_at";
const RESULT_COLUMNS: &str = "dataset_item_id, repetition, revision_id, input, \
    expected_output, trace_id, output, error, latency_ns, created_at";
const KEY_COLUMNS: &str = "dataset_item_id, repetition, trace_id, \
    error IS NOT NULL AS has_error, latency_ns";
/// The view's experiment columns: no `result_count` subquery, which would run once per row.
const VIEW_COLUMNS: &str = "e.id, e.name, e.dataset_id, e.dataset_version, i.dataset_item_id, \
    i.repetition, i.trace_id, i.input, i.expected_output, i.output, i.error, i.latency_ns, \
    i.created_at";
```

### Functions (exactly these signatures)

```rust
/// Inserts a `running` experiment.
pub(crate) async fn insert_tx(conn: &mut SqliteConnection, new: &NewExperiment) -> Result<(), MetaError>;

/// The experiment `id` of `project`, visible at `visible_from` (`created_at >= visible_from`
/// when set). `None` when absent or hidden.
pub(crate) async fn get(
    pool: &SqlitePool,
    project: ProjectId,
    id: Uuid,
    visible_from: Option<i64>,
) -> Result<Option<ExperimentRow>, MetaError>;

/// The same inside a write transaction.
pub(crate) async fn get_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    id: Uuid,
    visible_from: Option<i64>,
) -> Result<Option<ExperimentRow>, MetaError>;

/// One page of `project`'s visible experiments, newest first (`ORDER BY e.id DESC`; ids are
/// UUIDv7, so this is creation order). `dataset` keeps one dataset's experiments. `cursor` is the
/// last id of the previous page (`e.id < ?`). Returns the rows and the next cursor (the last
/// row's id when one more row exists).
pub(crate) async fn list(
    pool: &SqlitePool,
    project: ProjectId,
    dataset: Option<Uuid>,
    visible_from: Option<i64>,
    limit: u32,
    cursor: Option<Uuid>,
) -> Result<(Vec<ExperimentRow>, Option<Uuid>), MetaError>;

/// The visible experiments of `project` among `ids`, in no order.
pub(crate) async fn get_many(
    pool: &SqlitePool,
    project: ProjectId,
    ids: &[Uuid],
    visible_from: Option<i64>,
) -> Result<Vec<ExperimentRow>, MetaError>;

/// The ids among `ids` that are experiments of `project` (any status, hidden or not), for the
/// score write check (D-J-12).
pub(crate) async fn existing_ids(
    pool: &SqlitePool,
    project: ProjectId,
    ids: &[Uuid],
) -> Result<HashSet<Uuid>, MetaError>;

/// Live items of `dataset` at `version` (D-J-3's validity ranges), inside a write transaction
/// (the create route counts and inserts in one transaction, D-J-10).
pub(crate) async fn count_version_items_tx(
    conn: &mut SqliteConnection,
    dataset: Uuid,
    version: i64,
) -> Result<i64, MetaError>;

/// The pinned revisions of `items` in `dataset` at `version`, keyed by item id. An item with no
/// live revision at `version` is absent.
pub(crate) async fn pinned_items_tx(
    conn: &mut SqliteConnection,
    dataset: Uuid,
    version: i64,
    items: &[Uuid],
) -> Result<HashMap<Uuid, PinnedItem>, MetaError>;

/// Inserts or replaces results of `experiment` (one row per item and repetition, D-J-10).
/// `now` is each replaced or new row's `created_at`. Returns the rows written.
pub(crate) async fn upsert_results_tx(
    conn: &mut SqliteConnection,
    experiment: Uuid,
    results: &[NewResult],
    now: i64,
) -> Result<u64, MetaError>;

/// Sets `completed` (no `error`) or `failed` (with it) and `finished_at = now` when the
/// experiment is `running`. `Ok(false)` when it is not running.
pub(crate) async fn finish_tx(
    conn: &mut SqliteConnection,
    id: Uuid,
    error: Option<&str>,
    now: i64,
) -> Result<bool, MetaError>;

/// Deletes the experiment `id` of `project` and its results (FK cascade). False when absent.
pub(crate) async fn delete_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    id: Uuid,
) -> Result<bool, MetaError>;

/// The light projection of every result of `experiment`, ordered by `dataset_item_id`, then
/// `repetition`. At most `MAX_EXPERIMENT_ROWS` rows exist (the create check bounds them). No
/// text column is read (D-J-13).
pub(crate) async fn result_keys(
    pool: &SqlitePool,
    experiment: Uuid,
) -> Result<Vec<ResultKey>, MetaError>;

/// One page of full results of `experiment` in `(dataset_item_id, repetition)` order, strictly
/// after `after` when set; `errors_only` keeps rows with an error. At most `limit` rows; the
/// caller asks for one more than its page to know whether a next page exists.
pub(crate) async fn results_page(
    pool: &SqlitePool,
    experiment: Uuid,
    after: Option<(Uuid, i64)>,
    errors_only: bool,
    limit: i64,
) -> Result<Vec<ResultRow>, MetaError>;

/// Every full result (all repetitions) of `experiment` for the given items, ordered by
/// `dataset_item_id`, then `repetition`. At most 1,000 items (a compare page).
pub(crate) async fn item_rows(
    pool: &SqlitePool,
    experiment: Uuid,
    items: &[Uuid],
) -> Result<Vec<ResultRow>, MetaError>;

/// What `view_rows` returns.
pub(crate) enum ViewRows {
    Rows(Vec<ViewRow>),
    /// More than `max_rows` rows or more than `max_text_bytes` of text (D-J-15).
    TooLarge,
}

/// Every result row of the visible experiments of `project`, for the `experiment_items` SQL
/// view (J3.6). `window` keeps rows with `created_at ∈ [from, to)`. Rows are streamed
/// (`sqlx::query_as(..).fetch(pool)`); reading stops with `TooLarge` as soon as the rows exceed
/// `max_rows` or the summed byte lengths of `input`, `expected_output`, `output` and `error`
/// exceed `max_text_bytes`.
pub(crate) async fn view_rows(
    pool: &SqlitePool,
    project: ProjectId,
    visible_from: Option<i64>,
    window: Option<(i64, i64)>,
    max_rows: usize,
    max_text_bytes: usize,
) -> Result<ViewRows, MetaError>;
```

`view_rows` selects `VIEW_COLUMNS` from `experiment_items i JOIN experiments e ON e.id = i.experiment_id WHERE e.project_id = ?` (plus `AND e.created_at >= ?` when `visible_from` is set and `AND i.created_at >= ? AND i.created_at < ?` when `window` is set), `ORDER BY e.id, i.dataset_item_id, i.repetition`, decoded with one private record. It never runs the `result_count` subquery.

`results_page`: `SELECT {RESULT_COLUMNS} FROM experiment_items WHERE experiment_id = ?` plus `AND (dataset_item_id, repetition) > (?, ?)` when `after` is set (SQLite row values; bind the item id text and the repetition) and `AND error IS NOT NULL` when `errors_only`, `ORDER BY dataset_item_id, repetition LIMIT ?`. Item ids are UUIDv7 text of one fixed form, so text order is the id order. `item_rows`: `… WHERE experiment_id = ? AND dataset_item_id IN (…) ORDER BY dataset_item_id, repetition`.

SQL details:
- `get`/`get_tx`/`list`/`get_many`: `SELECT {EXPERIMENT_COLUMNS} FROM experiments e WHERE e.project_id = ? …`; the visibility clause is `AND e.created_at >= ?` only when `visible_from` is `Some`. Build the text with `sqlx::QueryBuilder` (as `saved_views.rs` builds its dynamic SQL); bind every value.
- `existing_ids`, `get_many`, `pinned_items_tx` and `item_rows` use `IN (…)` with one bind per id. Callers pass at most 1,000 ids (scores, a compare page) or 500 (results) or 4 (compare); no chunking needed. Above 1,000 ids return `MetaError::Corrupt { table: "experiments", column: "ids" }`: a misuse is a server bug, so it must be an internal error, never a 409 the client could see as its own fault.
- `count_version_items_tx`: `SELECT count(*) FROM dataset_items WHERE dataset_id = ?1 AND valid_from_version <= ?2 AND (valid_to_version IS NULL OR valid_to_version > ?2)`.
- `pinned_items_tx`: `SELECT item_id, revision_id, input, expected_output FROM dataset_items WHERE dataset_id = ? AND valid_from_version <= ? AND (valid_to_version IS NULL OR valid_to_version > ?) AND item_id IN (…)`.
- `upsert_results_tx`: `INSERT INTO experiment_items (experiment_id, dataset_item_id, repetition, revision_id, input, expected_output, trace_id, output, error, latency_ns, created_at) VALUES … ON CONFLICT (experiment_id, dataset_item_id, repetition) DO UPDATE SET revision_id = excluded.revision_id, input = excluded.input, expected_output = excluded.expected_output, trace_id = excluded.trace_id, output = excluded.output, error = excluded.error, latency_ns = excluded.latency_ns, created_at = excluded.created_at`. One statement per chunk of 50 rows (`push_values`; 11 binds per row stays under SQLite's bind limit). Sum `rows_affected`.
- `finish_tx`: `UPDATE experiments SET status = ?, error = ?, finished_at = ? WHERE id = ? AND status = 'running'`; true when one row changed.
- `delete_tx`: `DELETE FROM experiments WHERE id = ? AND project_id = ?`.

`json` columns are stored as given (callers serialize with `serde_json::to_string`).

### Tests

In `meta/experiments.rs`. Each test opens a test database, creates an org, a project and a dataset with 3 items at version 1 (M1's `insert_dataset` and `write_version`, in a local `async fn seed_dataset(db) -> Uuid`), then:

1. `insert_then_get_round_trips`: insert a `NewExperiment` (repetitions 2, item_count 3, metadata `{"model":"m"}`); `get` returns every field, `status == "running"`, `result_count == 0`, `finished_at == None`.
2. `get_hides_before_visible_from`: `created_at = 1_000`; `get(.., Some(1_001))` is `None`, `get(.., Some(1_000))` is `Some`, `get(.., None)` is `Some`.
3. `get_is_scoped_to_the_project`: the experiment of project A is `None` through project B.
4. `list_pages_newest_first`: insert 3 experiments with ids `Uuid::now_v7()` in order; `list(limit 2)` returns the last two (newest first) and a cursor; the next page returns the first and no cursor. `list(dataset: Some(other))` is empty.
5. `pinned_items_follow_the_version`: patch item 1 into version 2 with M1's `write_version` (close item 1, insert its new revision); `pinned_items_tx(version 1)` returns the version-1 revision of item 1; `version 2` returns the new revision; an unknown item id is absent. `count_version_items_tx` is 3 at both versions; after a third version deleting item 2 it is 2 at version 3 and still 3 at version 1.
6. `results_upsert_replaces_one_row_per_repetition`: upsert `(item1, 0)` and `(item1, 1)`, then `(item1, 0)` again with another output; `results_page(after None, errors_only false, limit 10)` has 2 rows, ordered by repetition, the first with the new output; `result_count` of `get` is 2; `result_keys` has the same 2 keys with `has_error == false`.
7. `finish_only_once`: `finish_tx(None)` is true and the row is `completed` with `finished_at`; a second call is false and changes nothing; on another experiment `finish_tx(Some("boom"))` gives `failed` with `error = "boom"`.
8. `delete_cascades_results`: after `delete_tx`, `results` is empty and `get` is `None`; deleting again is false.
9. `deleting_the_dataset_keeps_the_experiment`: `DELETE FROM datasets WHERE id = ?`; `get` returns the row with `dataset_id == None` and its results unchanged.
10. `existing_ids_ignores_other_projects`: two experiments in A, one in B; `existing_ids(A, [a1, a2, b1, random])` is `{a1, a2}`.
11. `view_rows_bounds`: two experiments with results at `created_at` 10 and 20; `window Some((15, 30))` returns only the second's rows; `visible_from Some(15)` hides the first experiment; `max_rows 1` with two rows is `TooLarge`; `max_text_bytes` one less than the rows' summed text lengths is `TooLarge`, and equal to it returns the rows.
12. `results_page_keyset_and_errors_only`: 3 items × 2 repetitions, the result `(item2, 1)` with an error; `results_page(after Some((item1, 1)), false, 2)` returns `(item2, 0)`, `(item2, 1)`; `results_page(None, true, 10)` returns only `(item2, 1)`; `item_rows([item3])` returns both repetitions of item 3; `result_keys` marks only `(item2, 1)` with `has_error`.
13. `content_is_not_in_debug`: `format!("{:?}", row)` of a `ResultRow` whose input is `"secret-input-1"` and output `"secret-output-1"` contains neither string.

### Done when

- The 13 tests pass; `just check` is green.

---

## J3.2 — Experiment scores

**Commit subject:** `server: link scores to experiments`

`ScoreCreate` gains `experiment_id` and `dataset_item_id` (D-J-12). `llmobs-core`'s `Score` already has both fields; it is not changed.

### Files

| File | Change |
|---|---|
| `crates/server/src/api/scores.rs` | two fields on `ScoreCreate` |
| `crates/server/src/scores.rs` | checks, the derived id's author, the stored fields; tests |
| every file with a `ScoreCreate {` literal | add `experiment_id: None, dataset_item_id: None` |
| `crates/server/openapi.json`, `web/src/api/schema.gen.ts` | regenerated |

### Read first

- `crates/server/src/api/scores.rs` `ScoreCreate`.
- `crates/server/src/scores.rs`: `write`, `check_item`, `field`, `invalid`, `derive_id`, `Writer::author`, the `Score { … }` build with `experiment_id: None, dataset_item_id: None`, and the tests module (`item`, `with_id`, `TestApp` use).
- `crates/server/src/meta/experiments.rs` `existing_ids` (J3.1).

### Steps

1. In `ScoreCreate`, after `metadata`, add:

```rust
    /// Experiment the score belongs to; set together with `dataset_item_id`, on a trace target
    /// (D-J-12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experiment_id: Option<Uuid>,
    /// Dataset item (stable id) the score is about; set together with `experiment_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset_item_id: Option<Uuid>,
```

2. Add `experiment_id: None, dataset_item_id: None` to every `ScoreCreate { … }` literal. Find them with:

```bash
grep -rn "ScoreCreate {" crates/ --include=*.rs
```

(today: `eval/backfill.rs`, `eval/execute.rs`, `routes/feedback.rs`, `routes/scores.rs` tests, `scores.rs` tests). A literal that ends with `..something` needs no edit. The destructuring `let ScoreCreate { … , .. } = item;` in `write` takes the two fields by name (step 4).

3. In `check_item`, after the target and name checks, push these errors (field paths through `field(index, …)`):
   - exactly one of the two set: `field(index, "experiment_id", "experiment_id and dataset_item_id go together")`;
   - both set and the target is not `ScoreTarget::Trace`: `FieldError { field: format!("scores[{index}].target"), message: "an experiment score needs a trace target".to_owned(), position: None }`.

4. In `write`, after the `check_item` loop and before `if !errors.is_empty()`: collect the distinct `experiment_id`s of the items (a `BTreeSet<Uuid>`); when non-empty, call `crate::meta::experiments::existing_ids(state.meta.reader(), project.id, &ids)` (a `MetaError` converts with `?`) and push `field(index, "experiment_id", "unknown experiment")` for every item whose id is missing. Hidden experiments (D-J-14) are accepted: a late SDK flush must not fail.

5. Derived ids: in the closure that computes `id` when `item.score_id` is `None`, use `item.experiment_id.unwrap_or(author)` as the author. Spec 06 says the author of an experiment score is the experiment (amended in step 8).

6. In the `Score { … }` build, take `experiment_id` and `dataset_item_id` from the item (add them to the destructuring) instead of `None`.

7. `just openapi`.

8. Spec 06 § Scores already says this (the plan PR amended it: experiment scores' derived id has `experiment_id` as its author, and a score may carry `experiment_id` and `dataset_item_id`). Check it does; if not, STOP and report.

### Tests (in `scores.rs`)

Each uses `TestApp::new()`, `seed()`, and inserts one experiment of the seeded project with `meta::experiments::insert_tx` through `app.state.meta.write` (a dataset first, through M1's store or SQL).

1. `experiment_fields_are_stored`: `write` one trace score with both fields through `Writer::Sdk { key_id }`; read it back with `llmobs_storage::score_list` (as the existing storage-read tests here do) and assert `experiment_id` and `dataset_item_id` columns equal the ids (as hyphenated lowercase text).
2. `experiment_fields_go_together`: only `experiment_id` → `400`, field `scores[0].experiment_id`, message `experiment_id and dataset_item_id go together`.
3. `experiment_scores_need_a_trace_target`: both on a span target → `400`, field `scores[0].target`.
4. `unknown_experiment_is_refused`: a random `experiment_id` → `400`, `scores[0].experiment_id`, `unknown experiment`; an experiment of another project → the same.
5. `derived_id_uses_the_experiment`: two writes without `score_id`, same trace, name and writer, one with experiment A and one with experiment B → two different ids; the same write twice with experiment A → the same id.
6. `scores_without_experiment_are_unchanged`: an existing test's item (`item("accuracy", 0.5)`) still answers the same derived id as before (the pinned `DERIVED` test stays green unchanged).
7. `hidden_experiment_scores_are_accepted` (D-J-14): insert the experiment with `created_at = app.state.clock.now_ns()`, set the org's `data_access_days` to 30 with `set_limits` (`crate::test_support`), advance the clock 31 days (`app.state.clock.advance`); a score write with the experiment's ids succeeds (`existing_ids` ignores the window, step 4).

### Done when

- The new tests pass, every existing score test passes unchanged except for the added `None` fields; `crates/server/openapi.json` shows the two new optional properties on `ScoreCreate`; `just check` is green.

---

## J3.3 — The experiment reads in storage

**Commit subject:** `storage: read an experiment's scores and traces`

Two reads in `llmobs-storage` (D-J-13). They follow the existing reads: a `QueryContext`, a timeout, `RunError`, and plain structs as `alerts.rs` and `metrics.rs` return. They apply no default exclusions (an experiment's traces are what they read).

### Files (edit only these)

| File | Change |
|---|---|
| `crates/storage/src/reads/experiments.rs` | new |
| `crates/storage/src/reads/mod.rs` | `mod experiments;` (alphabetical) and a `pub use experiments::{…}` line |
| `crates/storage/src/lib.rs` | add the new names to the existing `pub use reads::{…}` list, alphabetically |

### Read first

- `crates/storage/src/reads/scores.rs`: `score_list` (timeout pattern), `filters`, `hex_id`, the tests' `context()`, `json_rows`.
- `crates/storage/src/reads/deletions.rs`: `eval_trace_ids`/`eval_scan` (the `metadata['<key>']` expression built with `ctx.session().state().create_logical_expr(…, ctx.view_schema(ViewName::…))`, `execute_stream`, the `utf8` helper and `ctx.fail`).
- `crates/storage/src/views/catalog.rs` `TRACES` and `SCORES`: the view columns (`trace_id` is lowercase hex `Utf8` in both views; `duration_ns` `Int64`, `error_count` `Int64`, `cost_total` `Float64` nullable on `traces`; `experiment_id`, `dataset_item_id` `Utf8` nullable, `value_num` `Float64` nullable, `value_str` `Utf8` nullable, `deleted` `Boolean` on `scores`).
- `crates/storage/src/test_support.rs`: `score_row`, `live_scores_table`, `span_row`, `spans_table`, `traces_table`, `example_query`.
- `llmobs_core::names::metadata_key::EXPERIMENT_ID` (`llmobs.experiment_id`), `llmobs_core::schema::{scores, traces}`.

### Code

```rust
//! The scores and traces of one experiment, for its results and comparison (D-J-13).
//!
//! The `scores` view holds live scores only. Experiment scores carry `experiment_id`
//! (D-J-12); experiment traces carry `metadata['llmobs.experiment_id']` (spec 02 § Context
//! fields). Default exclusions do not apply.

/// Most score rows [`experiment_scores`] returns (CG-J2).
pub const MAX_EXPERIMENT_SCORES: usize = 500_000;
/// Most trace rows [`experiment_traces`] returns.
pub const MAX_EXPERIMENT_TRACES: usize = 100_000;

/// One live score of an experiment.
#[derive(Debug, Clone, PartialEq)]
pub struct ExperimentScore {
    /// Lowercase hex.
    pub trace_id: String,
    pub dataset_item_id: Option<String>,
    pub name: String,
    /// `numeric`, `boolean`, `categorical` or `text`.
    pub data_type: String,
    pub value_num: Option<f64>,
    pub value_str: Option<String>,
}

/// One trace of an experiment.
#[derive(Debug, Clone, PartialEq)]
pub struct ExperimentTrace {
    /// Lowercase hex.
    pub trace_id: String,
    pub cost_total: Option<f64>,
    pub duration_ns: i64,
    pub error_count: i64,
}

/// Live, not deleted scores whose `experiment_id` is `experiment` (a UUID, hyphenated; it is
/// compared as lowercase text), in no order.
///
/// # Errors
///
/// [`RunError::InvalidRequest`] when `experiment` is not a UUID or more than
/// [`MAX_EXPERIMENT_SCORES`] rows match (`experiment has more than 500000 scores`);
/// [`RunError::Timeout`]; other errors as `ctx.fail` classifies them.
pub async fn experiment_scores(
    ctx: &QueryContext,
    experiment: &str,
    since: i64,
    timeout: Duration,
) -> Result<Vec<ExperimentScore>, RunError>;

/// Traces whose merged `metadata['llmobs.experiment_id']` is `experiment`, in no order.
///
/// # Errors
///
/// As [`experiment_scores`], with [`MAX_EXPERIMENT_TRACES`]
/// (`experiment has more than 100000 traces`).
pub async fn experiment_traces(
    ctx: &QueryContext,
    experiment: &str,
    since: i64,
    timeout: Duration,
) -> Result<Vec<ExperimentTrace>, RunError>;
```

Implementation:
- Parse `experiment` with `uuid::Uuid::parse_str`; error `RunError::InvalidRequest("invalid experiment id".to_owned())`. Use `uuid.to_string()` as the literal.
- Wrap the body in `tokio::time::timeout(timeout, …)` exactly as `score_list` does.
- `since` (nanoseconds) is a lower time bound that lets DataFusion prune files: `experiment_scores` adds `col(scores::CREATED_AT).gt_eq(<since as the column's timestamp literal>)` and `experiment_traces` adds the same on `traces`' `start_time` (build the literal as `score_list`'s window does for `created_at`, and as `trace_list` does for `start_time`). The server passes the experiment's `created_at − CLOCK_SKEW_NS` (J3.5), because span start times and UUIDv7 score ids come from client clocks.
- `experiment_scores`: `ctx.session().table("scores")`, filter `col(scores::EXPERIMENT_ID).eq(lit(id)).and(col(scores::DELETED).eq(lit(false)))`, select `trace_id, dataset_item_id, name, data_type, value_num, value_str`, `limit(0, Some(MAX_EXPERIMENT_SCORES + 1))`, `execute_stream`, decode each batch with `as_string::<i32>()` / `as_primitive::<Float64Type>()` (cast to `Utf8` first with the `utf8` helper pattern of `deletions.rs` if the column is a view or dictionary type). Rows with a null `trace_id` are skipped. More than the cap: the error.
- `experiment_traces`: build `metadata['llmobs.experiment_id']` over `ctx.view_schema(ViewName::Traces)` as `eval_scan` does over spans; filter it `.eq(lit(id))`; select `trace_id, cost_total, duration_ns, error_count`; same limit, stream and cap.
- The window of `QueryContext` (D101, `visible_from`) applies through the views; nothing else.

Re-export from `reads/mod.rs`: `pub use experiments::{ExperimentScore, ExperimentTrace, MAX_EXPERIMENT_SCORES, MAX_EXPERIMENT_TRACES, experiment_scores, experiment_traces};`, and the same names in `lib.rs`.

### Tests (in `reads/experiments.rs`)

A local `fn context_of(spans: &[Span], scores: &[Score]) -> QueryContext` builds a `ProjectQuery` like `example_query()` with `spans_table(spans)`, `traces_table(spans)` and `live_scores_table(scores)`. Experiment ids: `E1 = "01900000-0000-7000-8000-000000000e01"`, `E2 = "…0e02"`.

1. `scores_of_one_experiment`: scores `score_row(1, 0, …)` with `experiment_id = Some(E1)`, `dataset_item_id = Some(item)`, `value = Numeric(0.5)`; `score_row(2, 1, …)` with `E2`; `score_row(3, 2, …)` with none. `experiment_scores(E1)` returns exactly the first, with `trace_id` the 32-hex text of trace 0, `data_type "numeric"`, `value_num Some(0.5)`.
2. `deleted_and_replaced_scores_are_not_returned`: score 1 with `E1`, then a newer version of it with `deleted = true` → empty.
3. `categorical_scores_carry_the_label`: `ScoreValue::Categorical { label: "good", value: Some(1.0) }` → `value_str Some("good")`, `value_num Some(1.0)`.
4. `traces_of_one_experiment`: spans of trace 0 with `metadata["llmobs.experiment_id"] = E1` and `cost_total Some(0.25)` on one span, `status` error on another; trace 1 with `E2`; trace 2 without. `experiment_traces(E1)` returns one row: trace 0, `cost_total Some(0.25)` (the merged trace cost), `error_count 1`, `duration_ns` = the trace's `end − start`.
5. `bad_id_is_invalid_request`: `experiment_scores(ctx, "nope", …)` is `RunError::InvalidRequest`.
6. `no_match_is_empty`: an unknown UUID → empty vectors from both.
7. `since_bounds_both_reads`: the scores and traces of test 1 and test 4 with `since` one nanosecond after their `created_at` / `start_time` → empty; with `since` equal to it → the rows.

### Done when

- The 7 tests pass; `cargo test -p llmobs-storage reads::experiments` passes; `just check` is green.

---

## J3.4 — Experiment routes

**Commit subject:** `server: experiments API`

Create, list, get, delete, results and finish (the M3 rows of the design's API table except `results` and `compare`, which are J3.5).

### Files

| File | Change |
|---|---|
| `crates/server/src/api/experiments.rs` | new: limits, request and answer types |
| `crates/server/src/api/mod.rs` | `pub mod experiments;` |
| `crates/server/src/routes/experiments.rs` | new |
| `crates/server/src/routes/mod.rs` | `pub(crate) mod experiments;` |
| `crates/server/src/experiments/mod.rs` | new: module doc and `visible_from` only (§ Shared reference → Visibility); J3.5 adds the aggregation |
| `crates/server/src/lib.rs` (where the top-level modules are declared, next to M1's `datasets`) | `mod experiments;` |
| `crates/server/src/http/access.rs` | `routes.extend(crate::routes::experiments::routes());` next to the datasets line M1 added |
| `crates/server/src/api/openapi.rs` | `experiment_paths`, the `experiments` tag, the schemas |
| `crates/server/src/meta/experiments.rs` | remove the `#![cfg_attr(not(test), allow(dead_code))]` line |
| `crates/server/src/api_examples.rs` | the example rows (below) |
| `crates/server/tests/api_examples.rs` | the static files' types (as `static/score-definitions.json` is registered) |
| `fixtures/api/static/experiments.json`, `experiment.json`, `experiment-created.json`, `experiment-results-request.json` (in `fixtures/api/requests/`) | new, hand-written |
| `fixtures/api/README.md` | the `…0exx` range |
| `crates/server/openapi.json`, `web/src/api/schema.gen.ts`, `fixtures/api/manifest.json` | regenerated |

### Read first

- `crates/server/src/routes/score_definitions.rs` and `crates/server/src/routes/datasets.rs` (M1): `routes()`, `RouteDef`, the extractors (`State`, `Extension<Principal>`, `Extension<ProjectAccess>`, `error::Json`, `error::Path`, `error::Query`), the audit log line, `(StatusCode::CREATED, …)`.
- `crates/server/src/api/datasets.rs` (M1): `DatasetItem`, how it serializes JSON values and times; `routes::datasets::{find, created_by, item_json}`.
- `crates/server/src/api/common.rs`: `Page`, `Limit`, `FieldError`.
- `crates/server/src/api/openapi.rs`: `score_definition_paths`, `operation`, `with_query`, `project_param`, `path_uuid`, `json_body`, `json_of`, `json_named`, `responses`, `QUERY_ERRORS`, `collect_schemas` (`set.insert::<T>()`, `set.insert_page::<T>(…)`), `tag(…)` in `openapi()`; M1's `dataset_paths`.
- `crates/server/src/deletion/guard.rs` `blocking`.
- `crates/server/src/api_examples.rs` (the static rows) and `fixtures/api/README.md`.

### API types (`api/experiments.rs`)

Module doc: `//! Experiments API types (spec 09 § Datasets & experiments, D-J-10 to D-J-13).` Put the § Shared reference → Limits constants at the top. Every struct derives `Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema`; requests add `#[serde(deny_unknown_fields)]`; optional fields are `#[serde(default, skip_serializing_if = "Option::is_none")]`. Field docs on every field.

```rust
/// `running`, `completed` or `failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentStatus { Running, Completed, Failed }

pub struct Experiment {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub dataset_id: Option<Uuid>,
    pub dataset_name: String,
    pub dataset_version: i64,
    /// A JSON object.
    #[schema(value_type = Object)]
    pub metadata: serde_json::Map<String, serde_json::Value>,
    pub repetitions: i64,
    pub item_count: i64,
    pub result_count: i64,
    pub status: ExperimentStatus,
    pub error: Option<String>,
    #[serde(with = "llmobs_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: i64,
    /// Optional time, as `api/score_definitions.rs` writes one.
    pub finished_at: Option<i64>,
}

pub struct CreateExperimentRequest {
    pub name: String,
    /// Dataset id or name.
    pub dataset: String,
    pub dataset_version: Option<i64>,
    pub description: Option<String>,
    #[schema(value_type = Option<Object>)]
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
    pub repetitions: Option<i64>,
}

/// The created experiment and the first page of its pinned items. The SDK pages the rest with
/// `GET …/datasets/{dataset}/items?version=` from `next_cursor`.
pub struct CreateExperimentResponse {
    pub experiment: Experiment,
    pub items: Vec<DatasetItem>,
    /// Always present; `null` on the last page.
    pub next_cursor: Option<String>,
}

pub struct ExperimentResult {
    pub dataset_item_id: Uuid,
    pub repetition: i64,
    pub trace_id: Option<TraceId>,
    /// Any JSON value (a string, a number, an array, an object).
    #[schema(schema_with = crate::api::common::any_json)]
    pub output: Option<serde_json::Value>,
    pub error: Option<String>,
    pub latency_ns: Option<i64>,
}

pub struct ExperimentResultsRequest { pub results: Vec<ExperimentResult> }

pub struct ExperimentResultsResponse {
    /// Rows written.
    pub stored: i64,
    /// Rows not stored because their trace is being deleted (D-J-9).
    pub skipped: i64,
}

pub struct FinishExperimentRequest { pub error: Option<String> }

#[derive(…, IntoParams)]
pub struct ExperimentListParams {
    /// Keep one dataset's experiments (id or name).
    pub dataset: Option<String>,
    #[serde(default)] #[param(minimum = 1, maximum = 1000, default = 50)]
    pub limit: Limit,
    pub cursor: Option<String>,
}
```

Every input, expected-output and output field of M3 uses M1's `#[schema(schema_with = crate::api::common::any_json)]` (utoipa's AnyValue); `value_type = Object` stays only for `metadata`. A string output must round-trip through the request-example validator (test 5 below posts one).

`next_cursor` of `CreateExperimentResponse` is serialized even when `None` (no `skip_serializing_if`), as `Page` does. Deviation from the design's `items: Page<DatasetItem>`: the page's two fields sit flat in the answer, because a generic `Page<DatasetItem>` inside a struct has no OpenAPI name. Report it.

`fn experiment_of(row: &ExperimentRow) -> Result<Experiment, ApiError>` (in `api/experiments.rs` or `api/convert.rs`, as M1 placed `dataset_item`): parses `metadata` (`serde_json::from_str`; a failure is `ApiError::Internal` through `MetaError::Corrupt { table: "experiments", column: "metadata" }`) and maps `status` text to `ExperimentStatus` (unknown text: the same `Corrupt` error, column `status`).

### Routes (`routes/experiments.rs`)

```rust
pub(crate) fn routes() -> Vec<RouteDef>
```

| Method | Path | Access | Handler |
|---|---|---|---|
| GET | `/api/v1/projects/{project}/experiments` | `Project { perm: View, session_only: false }` | `list_experiments` |
| POST | `…/experiments` | `Project { perm: ManageDatasets, session_only: false }` | `create_experiment` (body limit 256 KiB: `post(…).layer(DefaultBodyLimit::max(256 * 1024))`) |
| GET | `…/experiments/{id}` | View | `get_experiment` |
| DELETE | `…/experiments/{id}` | ManageDatasets | `delete_experiment` |
| POST | `…/experiments/{id}/items` | ManageDatasets | `post_results` (body limit `RESULTS_BODY_LIMIT`, 40 MiB; the outputs are capped at 32 MiB per request by validation) |
| POST | `…/experiments/{id}/finish` | ManageDatasets | `finish_experiment` |

All `documented: true`. J3.5 adds `results` and `compare`; `compare` is a static segment, so axum routes it before `{id}`.

Handlers (every one but `delete_experiment` starts with `let visible = crate::experiments::visible_from(&state, access.project.org_id);` and treats a hidden experiment as `ApiError::NotFound`):

**`create_experiment`**
1. Validate the body; collect every error, then answer `ApiError::BadRequest { detail: Some("invalid experiment".to_owned()), errors }`:
   - `name`: trimmed, 1–`MAX_NAME_CHARS` characters (`chars().count()`), field `name`, `name must be 1 to 200 characters`;
   - `description`: at most `MAX_DESCRIPTION_CHARS` characters, field `description`;
   - `metadata`: `serde_json::to_string(&map)?.len() <= MAX_METADATA_BYTES`, field `metadata`, `metadata must be at most 64 KiB`;
   - `repetitions` (default 1): `1..=MAX_REPETITIONS`, field `repetitions`, `repetitions must be 1 to 20`.
2. Steps 3–5 run in **one** `state.meta.write` (D-J-10), so a dataset deleted between the read and the insert is a 404, never a foreign-key failure (500), and `dataset_name` is the name at insert time. Put them in a private `async fn create_tx(conn: &mut SqliteConnection, project: ProjectId, key: &DatasetKey, new: CreateInput) -> Result<Result<ExperimentRow, CreateRefusal>, MetaError>` (`CreateInput` holds the validated name, description, metadata text, repetitions, the requested version, id, `created_at` and `created_by`; `enum CreateRefusal { NotFound, Invalid(Vec<FieldError>) }`), and map it after the write: `NotFound` → `ApiError::NotFound`, `Invalid(errors)` → `ApiError::BadRequest { detail: Some("invalid experiment".to_owned()), errors }`. `key = DatasetKey::parse(&body.dataset)` (M1).
3. Inside `create_tx`: `let Some(dataset) = meta::datasets::get_dataset_tx(conn, project, &key).await? else { return Ok(Err(CreateRefusal::NotFound)) };`. `version = dataset_version.unwrap_or(dataset.current_version)`; `current_version == 0` → field `dataset`, `the dataset has no items`; else outside `1..=current_version` → field `dataset_version`, `dataset_version must be 1 to <current_version>`.
4. `item_count = meta::experiments::count_version_items_tx(conn, dataset.id, version)`; `0` → field `dataset_version`, `the version has no items`; `item_count * repetitions > MAX_EXPERIMENT_ROWS` → field `repetitions`, `items × repetitions must be at most 20000`.
5. `insert_tx` (`id = Uuid::now_v7()`, `created_at = state.clock.now_ns()`, `dataset_name = dataset.name`, `metadata` serialized, `created_by = routes::datasets::created_by(&principal)`), then `get_tx(conn, project, id, None)` (`None` → `MetaError::Corrupt { table: "experiments", column: "id" }`).
6. After the write, read the first page: `meta::datasets::list_items(reader, dataset.id, version, None, CREATE_ITEMS_PAGE)`; it returns at most `CREATE_ITEMS_PAGE + 1` rows: keep the first `CREATE_ITEMS_PAGE`, and when there was one more, `next_cursor` is the last kept row's `item_id` as text (the cursor `GET …/items` takes; check M1's `list_items` route uses the item id as its cursor, and use whatever text form it uses). Map with `routes::datasets::item_json`.
7. Audit: `tracing::info!(target: "audit", experiment_id = %id, dataset_id = %dataset.id, project_id = %project, actor = %created_by_text, "experiment created")` (`actor` is the user id or `key:<id>`).
8. Answer `201 CreateExperimentResponse` with the row `create_tx` returned.

**`list_experiments`**: `dataset` resolved with `routes::datasets::find` (unknown → 404); `cursor` parsed as a UUID (bad → 400 field `cursor`, `invalid cursor`); `list(…, limit.get(), cursor)`; answer `Page<Experiment>` with `next_cursor = next.map(|id| id.to_string())`.

**`get_experiment`**: `get(…)` or 404.

**`delete_experiment`**: `delete_tx` in a write; absent → 404; **the window does not apply** (D-J-14: deleting reveals nothing, and the rows hold outputs), so a hidden experiment can be deleted; audit `"experiment deleted"`; `204`.

**`post_results`**
1. `results.len()` in `1..=MAX_RESULTS_PER_REQUEST` else 400 field `results`, `between 1 and 500 results per request`.
2. Per result `i`: `repetition` in `0..repetitions` (checked after loading the experiment) → `results[i].repetition`, `repetition must be 0 to <repetitions − 1>`; `output` serialized ≤ `MAX_OUTPUT_BYTES` → `results[i].output`, `output must be at most 256 KiB`; `error` ≤ `MAX_ERROR_CHARS` characters → `results[i].error`; `latency_ns` ≥ 0 → `results[i].latency_ns`; a repeated `(dataset_item_id, repetition)` in the batch → `results[i].dataset_item_id`, `duplicate item and repetition`. The serialized outputs of the whole request total at most `MAX_OUTPUTS_PER_REQUEST` → field `results`, `outputs must total at most 32 MiB per request` (D-J-10). Serialize each output once here and keep the text for the write.
3. `let now = state.clock.now_ns();` and `let state2 = state.clone();` (`AppState` is `Clone`) for the closure.
4. In one `state.meta.write`: `get_tx` (absent or hidden → `NotFound`); then **the deletion guard (D-J-9), inside the transaction**: for each result with a `trace_id`, `state2.deletions.guard.blocking(project, trace, &[], now)`; a `Some(_)` drops the result and counts it as skipped. The check must stay after `get_tx` and inside the closure: a deletion's resolve write (J3.7's cascade) is serialized with this transaction, so a deletion whose guard entry exists before this check is refused here, and one registered after it runs its cascade after this commit and deletes the rows. Put a comment saying so above the check. Then: `status != "running"` → `MetaError::Conflict("the experiment is finished")` (409); `dataset_id == None` → `MetaError::Conflict("the experiment's dataset was deleted")`; `pinned_items_tx(dataset, dataset_version, ids)`; an item missing → collect `results[i].dataset_item_id`, `the item is not in the pinned version` and return a 400 (return the errors out of the closure as a typed value, then map to `ApiError::BadRequest`; a `MetaError` cannot carry field errors). Build `NewResult`s with the pinned revision's `revision_id`, `input`, `expected_output`, and `upsert_results_tx(…, now)`.
5. No audit line per batch (it is a hot path); `tracing::debug!(experiment_id = %id, stored, skipped, "experiment results stored")`.
6. Answer `200 ExperimentResultsResponse { stored, skipped }`.

The step-2 checks that need the experiment (repetition) run inside the write after `get_tx`; the others run before it.

**`finish_experiment`**: `error` ≤ `MAX_ERROR_CHARS` characters (400 field `error`); write: `get_tx` (404), `finish_tx`; when `false` (not running), compare the stored status with the requested outcome (`completed` without `error`, `failed` with one): the same → answer the stored row unchanged, `200`, no audit line (an SDK retry after a lost answer, D-J-10); another → `MetaError::Conflict("the experiment is finished")` (409). After a real finish: audit `"experiment finished"` with `status`; answer the row (`get_tx` again inside the write).

### OpenAPI

`experiment_paths(paths)` after `dataset_paths` in `paths()`, tag `experiments` ("Experiments run by the SDK and their results."), operation ids: `list_experiments`, `post_experiment`, `get_experiment`, `delete_experiment`, `post_experiment_results`, `post_experiment_finish`. Error lists: reads `&[400, 401, 403, 404, 429]`; create `&[400, 401, 403, 404, 413, 429]`; results `&[400, 401, 403, 404, 409, 413, 429]`; finish `&[400, 401, 403, 404, 409, 429]`; delete `&[401, 403, 404, 429]`. Descriptions say who may call: "Any project member or a `read` key." / "Project members and above, or a `write` key." Register `Experiment`, `ExperimentStatus`, `CreateExperimentRequest`, `CreateExperimentResponse`, `ExperimentResult`, `ExperimentResultsRequest`, `ExperimentResultsResponse`, `FinishExperimentRequest`, and `set.insert_page::<Experiment>("ExperimentPage")` in `collect_schemas`.

### Fixtures

Hand-written, canonical (pretty JSON, 2 spaces, one trailing newline):
- `static/experiment.json`: experiment `…0e01`, name `baseline`, dataset `…0c01` `faq` version 2, `metadata {"model": "gpt-5-mini", "prompt": "v1"}`, repetitions 1, item_count 3, result_count 3, `completed`, `created_at "2026-01-01T10:00:00.000000000Z"`, `finished_at "2026-01-01T10:05:00.000000000Z"`.
- `static/experiments.json`: a page with `…0e02` (`candidate`, `{"model": "gpt-5-mini", "prompt": "v2"}`, created 11:00, finished 11:05) then `…0e01`, `next_cursor: null`.
- `static/experiment-created.json`: `…0e02` as `running` with `result_count 0` and no `finished_at`, plus `items` = the three items of `faq` at version 2 exactly as M1's generated `fixtures/api/responses/datasets/01900000-0000-7000-8000-000000000c01/items.json` holds them (copy its `data` objects), `next_cursor: null`.
- `requests/experiment-results.json`: one `ExperimentResultsRequest` with three results (items `…0d01`–`…0d03`, repetition 0, trace ids `e1000000000000000000000000000001` … `…03`, small outputs, latencies 1–3 s).

Manifest rows in `api_examples.rs` (as the score-definition rows): `list_experiments` → `static/experiments.json`; `get_experiment` with `id` `…0e01` → `static/experiment.json`; `example_created("post_experiment", …, "static/experiment-created.json")`. Register each static file's Rust type in `crates/server/tests/api_examples.rs` where `static/score-definitions.json` is registered (`Page<Experiment>`, `Experiment`, `CreateExperimentResponse`), and the request file where other `requests/` files are. Add the `…0exx` row to `fixtures/api/README.md`'s id table. Run `just openapi` then `just api-examples`.

### Tests (`routes/experiments.rs`)

Helper `async fn seeded_dataset(app, seeded) -> Uuid`: creates a dataset `faq` (no schema) with 3 items through `POST …/datasets` and `POST …/datasets/faq/items` with the seeded cookie (version 1). Helper `async fn create(app, seeded, body: Value) -> TestResponse`.

1. `create_pins_the_version_and_returns_items`: create `{name: "run", dataset: "faq"}` → 201; `experiment.dataset_version == 1`, `item_count == 3`, `status == "running"`, `items.len() == 3`, `next_cursor == null`. Add an item (version 2); create again with `dataset_version: 1` → `item_count == 3`; without → 4.
2. `create_validates`: empty name, `repetitions: 21`, `dataset_version: 9`, metadata over 64 KiB → 400 with fields `name`, `repetitions`, `dataset_version`, `metadata` (one request per case); unknown dataset → 404; `repetitions: 20` with 1,001 items → 400 field `repetitions` (insert the items with the store in one write to keep the test fast).
3. `results_snapshot_the_pinned_revision`: create; patch item 1 to a new input (version 2); post a result for item 1 → `stored 1`; `meta::experiments::results_page(reader, id, None, false, 10)` shows the version-1 input.
4. `results_replace_per_repetition`: post `(item1, 0)` twice with different outputs → one row with the second output; `result_count == 1`.
5. `results_validate`: repetition 1 when repetitions is 1 → 400 `results[0].repetition`; an item of another dataset → 400 `results[0].dataset_item_id`; 501 results → 400 `results`; duplicate pair → 400; output of 300 KiB → 400 `results[0].output`; a string output (`"output": "Paris"`) is accepted and stored as `"\"Paris\""` (any JSON, not only objects).
6. `finish_then_results_conflict`: finish → `completed`; finish again without `error` → `200` with the same `finished_at`; finish with `{error: "late"}` → 409; post results → 409; a new experiment finished with `{error: "crash"}` → `failed`, `error == "crash"`; finishing it again with `{error: "crash"}` → `200`, and without `error` → 409.
6b. `outputs_are_capped_per_request`: an experiment with `repetitions` 20 on 10 items (200 distinct `(item, repetition)` pairs); one request of 170 results whose outputs are JSON strings of 200 KiB each (34,000 KiB in total: over the 32 MiB cap, under the 40 MiB body limit) → 400 field `results`, message `outputs must total at most 32 MiB per request`; the first 160 of them (32,000 KiB) → 200, `stored 160`.
6c. `create_on_a_deleted_dataset_is_not_found`: seed `faq`; delete its row with SQL in a `state.meta.write`; call `create_tx` directly with `DatasetKey::Id(faq id)` inside a `state.meta.write` → `Ok(Err(CreateRefusal::NotFound))`, and `POST …/experiments {dataset: "<faq id>"}` → 404 (never 500).
7. `blocked_traces_are_skipped`: add the trace id to `app.state.deletions.guard` (as `deletion/mod.rs` tests do with `add_traces`); post two results, one on that trace → `stored 1, skipped 1`; `result_keys` holds only the other one. (The order "guard after `get_tx`, inside the write" is checked by review of the code and its comment; no test can interleave the deletion's write.)
8. `list_filters_and_pages`: three experiments, two on `faq` and one on a second dataset; `?dataset=faq` returns two, newest first; `?limit=1` gives a cursor that returns the next.
9. `hidden_experiments_are_absent` (D-J-14): set the org's `data_access_days` to 30 with `set_limits` (`crate::test_support`), create an experiment, advance the clock 31 days (`app.state.clock.advance`); `GET` → 404, list → empty, results post → 404, finish → 404; `DELETE` → 204 (the window does not apply to deleting), and the row is gone (`meta::experiments::get(.., None)` is `None`).
10. `deleting_the_dataset_keeps_experiments`: `DELETE …/datasets/faq` → the experiment's `dataset_id` is absent, `dataset_name == "faq"`; posting results → 409.
11. `delete_removes_the_experiment`: `DELETE` → 204; `GET` → 404.
12. `keys_can_read_and_write`: `seeded.read_key` can `GET` the list and the experiment, and gets 403 on `POST`; `app.write_key(&seeded)` can create.
12b. `roles_and_tokens`: a viewer session gets 403 on create (`ManageDatasets` needs member); a read-write personal access token of a member creates (201); a read-only personal access token gets 403 on create (as `http/access.rs` refuses a read-only PAT on a non-GET; use the PAT helper `test_support` already has, e.g. `personal_token`).
13. `logs_carry_no_content` (`#[tokio::test]`): capture logs (`capture_logs` from `test_support`) around create (name `secret-name-1`, metadata value `secret-meta-1`), results (output `secret-output-1`, error `secret-error-1`) and finish; none of the four strings appears.

The access suite (`http/access_suite.rs`) picks up the new routes by itself; run it (`cargo test -p llmobs-server access_suite`) and fix nothing but real failures. If it fails because a body is required, STOP and report.

### Done when

- All tests above and the access suite pass; `openapi.json`, `schema.gen.ts` and the manifest are regenerated and committed; `just check` is green.

---

## J3.5 — Results and comparison

**Commit subject:** `server: experiment results and comparison`

### Files

| File | Change |
|---|---|
| `crates/server/src/experiments/mod.rs` | add the pure aggregation (J3.4 created the module with `visible_from`) |
| `crates/server/src/api/experiments.rs` | the result and comparison types |
| `crates/server/src/routes/experiments.rs` | two routes |
| `crates/server/src/api/openapi.rs`, `api_examples.rs`, `tests/api_examples.rs` | paths, schemas, examples |
| `fixtures/api/static/experiment-results.json`, `experiment-comparison.json` | new |
| generated files | regenerated |

### API types (add to `api/experiments.rs`)

```rust
/// One score of a result row.
pub struct ResultScore {
    pub data_type: ScoreDataType,          // llmobs_core::enums::ScoreDataType
    pub value_num: Option<f64>,
    pub value_str: Option<String>,
}

pub struct ScoreSummary {
    pub name: String,
    pub data_type: ScoreDataType,
    /// Rows with a value of this score.
    pub count: i64,
    pub mean: Option<f64>,
    pub median: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// Booleans only: the mean of 0/1.
    pub pass_rate: Option<f64>,
    /// Categoricals only: rows per label.
    pub labels: BTreeMap<String, i64>,
}

pub struct ExperimentSummary {
    pub experiment: Experiment,
    pub scores: Vec<ScoreSummary>,          // by name
    pub result_count: i64,
    pub error_count: i64,
    pub latency_mean_ns: Option<i64>,
    pub latency_p50_ns: Option<i64>,
    pub latency_p95_ns: Option<i64>,
    pub cost_total: Option<f64>,
}

pub struct ResultRow {
    pub dataset_item_id: Uuid,
    pub repetition: i64,
    #[schema(schema_with = any_json)] pub input: serde_json::Value,
    #[schema(schema_with = any_json)] pub expected_output: Option<serde_json::Value>,
    #[schema(schema_with = any_json)] pub output: Option<serde_json::Value>,
    pub error: Option<String>,
    pub trace_id: Option<TraceId>,
    pub latency_ns: Option<i64>,
    pub cost_total: Option<f64>,
    pub scores: BTreeMap<String, ResultScore>,
}

pub struct ExperimentResults {
    pub summary: ExperimentSummary,
    pub rows: Vec<ResultRow>,
    pub next_cursor: Option<String>,        // always serialized
}

pub struct CompareCell {
    pub experiment_id: Uuid,
    #[schema(schema_with = any_json)] pub output: Option<serde_json::Value>,
    pub error: Option<String>,
    pub trace_id: Option<TraceId>,
    pub latency_ns: Option<i64>,
    /// Mean over repetitions, per score name.
    pub scores: BTreeMap<String, f64>,
    pub regressed: bool,
}

pub struct CompareRow {
    pub dataset_item_id: Uuid,
    #[schema(schema_with = any_json)] pub input: serde_json::Value,
    #[schema(schema_with = any_json)] pub expected_output: Option<serde_json::Value>,
    /// One cell per experiment that has a result for this item, in request order. An
    /// experiment without a result for the item has no cell: match cells by `experiment_id`,
    /// never by position (D-J-13).
    pub cells: Vec<CompareCell>,
}

/// One score's delta of a candidate to the baseline.
pub struct ScoreDelta {
    /// Candidate mean minus baseline mean, over the compared items.
    pub delta: f64,
    /// Items that both experiments ran without error and scored with this name.
    pub n_compared: i64,
}

pub struct ScoreDeltas {
    pub experiment_id: Uuid,
    /// Per score name with at least one compared item.
    pub scores: BTreeMap<String, ScoreDelta>,
}

pub struct ExperimentComparison {
    pub experiments: Vec<ExperimentSummary>,   // request order; the first is the baseline
    pub deltas: Vec<ScoreDeltas>,              // candidates only
    pub warnings: Vec<String>,
    pub rows: Vec<CompareRow>,
    pub next_cursor: Option<String>,           // always serialized
}

#[derive(…, IntoParams)]
pub struct ResultsParams {
    #[serde(default)] pub errors_only: bool,
    #[serde(default)] #[param(minimum = 1, maximum = 1000, default = 50)] pub limit: Limit,
    pub cursor: Option<String>,
}

#[derive(…, IntoParams)]
pub struct CompareParams {
    /// 2 to 4 experiment ids, comma-separated; the first is the baseline.
    pub ids: String,
    /// Regression threshold, absolute, 0 to 1000. Default 0.1.
    pub threshold: Option<f64>,
    #[serde(default)] pub regressions_only: bool,
    #[serde(default)] #[param(minimum = 1, maximum = 1000, default = 50)] pub limit: Limit,
    pub cursor: Option<String>,
}
```

`any_json` is `crate::api::common::any_json` (M1). The design calls `ResultScore` `ScoreValue`; it is renamed because `llmobs_core::score::ScoreValue` already exists in the OpenAPI document. Report it. Check `ScoreDataType` is a `ToSchema` already registered (it is used by score definitions); if it is not, STOP.

### Aggregation (`experiments/mod.rs`, pure functions)

Module doc: `//! Results and comparison of experiments, computed in memory from meta.db rows and two storage reads (D-J-13).`

Inputs:

```rust
pub(crate) struct Loaded {
    pub(crate) experiment: ExperimentRow,
    /// The light projection of every result, ordered by item, repetition (D-J-13).
    pub(crate) keys: Vec<meta::experiments::ResultKey>,
    pub(crate) scores: Vec<llmobs_storage::ExperimentScore>,
    pub(crate) traces: Vec<llmobs_storage::ExperimentTrace>,
}
```

Every aggregate (summaries, regressions, deltas, paging decisions) is computed from `keys`, `scores` and `traces`. The text columns (input, expected output, output, error text) are read from `meta.db` only for the rows or items of the answered page (`results_page`, `item_rows`), so a request holds at most one page of text.

Rules, exactly:

1. **Scores of a row.** A "row" below is a `ResultKey` unless the rule says full row. Index scores by `trace_id`. A row's scores are the scores whose `trace_id` equals the row's trace id (hex). A row without a trace has none. Scores whose trace is in no row are ignored.
2. **One data type per name.** For each name, count scores per `data_type` over the whole experiment; keep the type with the most scores (ties: the alphabetically first type text); scores of that name with another type are ignored.
3. **Value of a row for a name** (`row_value`): the mean of the kept scores' `value_num` on that row (several scores of one name on one trace are averaged); `None` when none has a `value_num`. For the `ResultScore` of a row: `value_num` = that mean, `value_str` = the first kept score's `value_str` (scores sorted by `value_str` for determinism), `data_type` = the kept type.
4. **ScoreSummary** per kept name, sorted by name: `count` = rows with at least one kept score of the name; over the rows' `row_value`s that are `Some`: `mean` (arithmetic), `median` (sort ascending; odd n: the middle; even n: the mean of the two middle values), `min`, `max`; for `boolean`: `pass_rate = mean`; for `categorical`: `labels` counts each row's `value_str` (rows without a label are not counted); for `text`: only `count` (mean, median, min, max, pass_rate are `None`). Empty sets give `None`.
5. **Experiment numbers.** `result_count` = rows; `error_count` = rows with `has_error`; latency over the rows' `Some(latency_ns)`: `latency_mean_ns` = the integer mean (sum as `i128`, divided, truncated); `latency_p50_ns` and `latency_p95_ns` by nearest rank: sort ascending, index `ceil(p × n) − 1` clamped to `0..n` (`p` 0.5 and 0.95). `cost_total` = sum of `cost_total` over the traces whose id is a row's trace and whose cost is `Some`; `None` when no such trace has a cost.
6. **Result rows.** The page comes from `meta::experiments::results_page` (full rows); one `ResultRow` per full row, JSON fields parsed from text (`serde_json::from_str`; a failure is `MetaError::Corrupt { table: "experiment_items", column: … }`), `scores` by rules 1–3 from the row's trace, `cost_total` from the trace index.
7. **Results paging.** The cursor is the text `"<item uuid>:<repetition>"` (a malformed cursor is a 400 field `cursor`, `invalid cursor`); the route calls `results_page(after = the cursor, errors_only, limit + 1)`; `next_cursor` is the last returned row's key when `limit + 1` rows came back (the extra row is dropped).
8. **Comparison cells.** For each experiment, per item it has at least one row for: `cell.scores[name]` = mean over the item's rows of `row_value` (rows with `None` skipped; names with no value left out); `has_error` of the cell = any of the item's rows has an error. An experiment with no row for an item has **no cell** for it; cells are listed in request order and matched by `experiment_id`. For the answered page only, from `item_rows`: `output`, `error`, `trace_id`, `latency_ns` are the item's lowest repetition's; when another repetition has an error and the lowest does not, `error` is that error. An item's `input` and `expected_output` come from the first experiment (request order) that has a row for it.
9. **Regression.** For a candidate cell (not the baseline's) whose item has a baseline cell: `regressed` when, for some name in both cells, `candidate < baseline − threshold`, or when the baseline cell has no error and the candidate cell has one. The baseline cell's `regressed` is `false`. No baseline cell: `false`.
10. **Deltas** (D-J-13). For each candidate and each score name: the compared items are the items where both the baseline and the candidate have a cell without error and with a value for the name (rule 8). `n_compared` = their count; `delta` = (mean of the candidate's cell values over them) − (mean of the baseline's cell values over them). A name with `n_compared == 0` is left out. So a candidate that fails on hard items is not credited with a better mean.
11. **Warnings.** `"experiments use different dataset versions"` when the versions differ; `"<n> items have no result in the baseline"` when candidates have items the baseline lacks (n > 0); for each candidate, `"<candidate id>: <n> items are left out of the deltas (missing or failed in one of the two)"`, where n = |items with a cell in the baseline or the candidate| − |items with a cell without error in both|, when n > 0.
12. **Compare paging.** Rows are the union of item ids (from `keys`), sorted by item id; `regressions_only` keeps rows with at least one regressed cell (decided from `keys` and scores); the cursor is the last item id (`invalid cursor` as above); `limit`. The route then reads `item_rows(experiment, page items)` for each experiment to fill the page's text fields.

Functions:

```rust
pub(crate) fn summary(loaded: &Loaded) -> Result<ExperimentSummary, ApiError>;
/// Parses a results cursor `"<item uuid>:<repetition>"` (400 field `cursor` when malformed).
pub(crate) fn parse_results_cursor(text: &str) -> Result<(Uuid, i64), ApiError>;
/// The answered page's rows (rule 6) from `results_page`'s full rows.
pub(crate) fn result_rows(loaded: &Loaded, page: &[meta::experiments::ResultRow]) -> Result<Vec<ResultRow>, ApiError>;
/// Rules 8, 9 and 12: the page's item ids and the next cursor, from `keys` and scores only.
pub(crate) fn compare_items(loaded: &[Loaded], threshold: f64, regressions_only: bool, limit: usize, cursor: Option<&str>) -> Result<(Vec<Uuid>, Option<String>), ApiError>;
/// The whole comparison: summaries, deltas, warnings (rules 10, 11) and the page's rows.
/// `texts[i]` holds `item_rows` of experiment `i` for `items`.
pub(crate) fn compare(loaded: &[Loaded], texts: &[Vec<meta::experiments::ResultRow>], items: &[Uuid], threshold: f64, next_cursor: Option<String>) -> Result<ExperimentComparison, ApiError>;
```

`compare` calls `summary` for each.

### Routes (add to `routes/experiments.rs`)

| Method | Path | Access | Handler |
|---|---|---|---|
| GET | `…/experiments/{id}/results` | View | `get_results` |
| GET | `…/experiments/compare` | View | `compare_experiments` |

`async fn load(state, access, row: ExperimentRow, ctx: &QueryContext, deadline: Deadline) -> Result<Loaded, ApiError>`: `meta::experiments::result_keys(reader, row.id)`, `llmobs_storage::experiment_scores(ctx, &row.id.to_string(), row.created_at.saturating_sub(CLOCK_SKEW_NS), deadline.remaining()?)` and `experiment_traces(…)` with the same bound. Open the context once per request with `routes::read::open_query(&state, org, project, None)` and `Deadline::start()`; map a storage `RunError` with `ApiError::from` (as `routes/traces.rs` does).

- `get_results`: `get` (404); `load`; `summary`; parse the cursor; `results_page(reader, id, after, errors_only, limit + 1)`; `result_rows` on the kept rows; answer `ExperimentResults`.
- `compare_experiments`: split `ids` on `,`, trim, parse each as a UUID (400 field `ids`, `ids must be 2 to 4 experiment ids`), 2–4 distinct ids (a repeated id: the same message); `threshold` default 0.1, must be finite and in `0..=1000` (400 field `threshold`, `threshold must be 0 to 1000`; `NaN`, `inf`, negative and over 1000 are refused); `get_many` (any missing or hidden → 404); all must share one `dataset_id` that is `Some` (400 field `ids`, `the experiments must use the same dataset`); keep request order; `load` each; `compare_items`; `item_rows` for each experiment; `compare`.

### OpenAPI, fixtures

Operation ids `get_experiment_results` and `compare_experiments`, error lists `&[400, 401, 403, 404, 422, 429, 503]` (the storage read can time out or be busy; check how `trace_paths` lists them and use the same list). Register the new types in `collect_schemas`. Hand-written `static/experiment-results.json` (summary of `…0e01` with scores `exact` (boolean, pass_rate 0.666…) and `quality` (numeric), three rows with traces `e100…01`–`…03`) and `static/experiment-comparison.json` (`…0e01` vs `…0e02`, item `…0d02` regressed in the candidate; deltas as `{delta, n_compared}`); manifest rows `get_experiment_results` (id `…0e01`) and `compare_experiments` (query `ids=…0e01,…0e02`); type registrations. These are static because the example dataset in `fixtures/api/dataset/` has no experiment traces; the values only have to round-trip and follow the rules above (compute them by hand and check `pass_rate` = 2/3 written as `serde_json` prints it after a round trip, which `canonical_json` enforces).

### Tests

Unit tests in `experiments/mod.rs` (no database), with a builder `fn loaded(rows: &[(item: u128, rep: i64, trace: Option<&str>, has_error: bool, latency: Option<i64>)], scores: &[(trace, name, type, num, str)], traces: &[(trace, cost)]) -> Loaded`:

1. `median_and_percentiles`: values 1, 2, 3, 4 → median 2.5, mean 2.5, min 1, max 4; latencies 10, 20, 30, 40, 50, 60, 70, 80, 90, 100 → p50 50, p95 100, mean 55.
2. `boolean_pass_rate`: three rows true, true, false → `pass_rate` ≈ 0.6667, `mean` the same.
3. `categorical_labels`: labels `good`, `good`, `bad` (with `value_num` 1, 1, 0) → `labels {"bad":1,"good":2}`, mean ≈ 0.6667; without `value_num` → mean `None`.
4. `text_scores_count_only`.
5. `majority_type_wins`: name `q` with 3 numeric and 1 text → numeric, `count == 3`.
6. `several_scores_on_one_trace_average`.
7. `cost_sums_row_traces_only`: a trace of the experiment not in any row is not counted; all costs `None` → `None`.
8. `results_cursor_parsing`: `parse_results_cursor("<uuid>:3")` is `(uuid, 3)`; `"x"`, `"<uuid>"`, `"<uuid>:-1"` and `"<uuid>:a"` → 400 field `cursor`. (Paging itself is `results_page`, tested in J3.1, and test 12b below.)
9. `comparison_regressions`: baseline `exact` 1.0 on item A, candidate 0.0 → A regressed; candidate 0.95 vs baseline 1.0 with threshold 0.1 → not regressed; baseline no error, candidate error → regressed; `regressions_only` keeps only regressed items.
10. `comparison_repetitions_average`: candidate with 2 repetitions 1 and 0 → cell mean 0.5.
11. `comparison_warnings_and_deltas`: versions 1 and 2 → the warning; an item only in the candidate → `1 items have no result in the baseline`; delta = candidate mean − baseline mean over the compared items, with `n_compared`.
11b. `deltas_ignore_failed_items`: baseline scores `exact` 1 on A and 0 on B; the candidate scores 1 on A and fails on B (an error, no score) → `deltas[0].scores.exact == {delta: 0.0, n_compared: 1}` (not +0.5), the warning `"<candidate id>: 1 items are left out of the deltas (missing or failed in one of the two)"`, and B's candidate cell is `regressed`.
11c. `cells_only_for_experiments_with_a_result`: three experiments, the second has no row for item C → C's row has two cells, with the first and third ids, in that order.

HTTP tests in `routes/experiments.rs` (data through the real store and `app.commit_spans` / `scores::write`):

12. `results_join_scores_and_traces`: create an experiment on `faq` (3 items); commit one span per item with `metadata["llmobs.experiment_id"] = <id>`, `environment = Some("experiment")`, `cost_total = Some(0.01)`, `ingest_time = now`; write a boolean score `exact` per trace with the experiment and item ids (`scores::write` with `Writer::Api`); post three results with those traces; `GET …/results` → `result_count 3`, `scores[0].name == "exact"`, `cost_total` ≈ 0.03, each row has `scores.exact`.
12b. `results_pages_over_http`: 5 results, 2 with errors; `?limit=2` gives a cursor that returns the next 2, then the last with `next_cursor: null`; `?errors_only=true` returns 2; `?cursor=x` → 400.
13. `compare_needs_the_same_dataset`: two experiments on two datasets → 400 field `ids`; one id → 400; five ids → 400; the same id twice (`ids=a,a`) → 400 field `ids`; a malformed id (`ids=a,nope`) → 400 field `ids`; an unknown id → 404.
13b. `compare_threshold_bounds`: `threshold=-0.1`, `threshold=1000.5`, `threshold=NaN` and `threshold=inf` → 400 field `threshold`; `threshold=0` and `threshold=1000` → 200.
14. `compare_marks_regressions`: two experiments on `faq`, baseline scores 1, 1, 1 and candidate 1, 0, 1 → the candidate's cell of the second item (found by `experiment_id`) has `regressed == true`, `deltas[0].scores.exact.delta` ≈ −0.3333 and `n_compared == 3`.
15. `hidden_experiments_cannot_be_compared` (D-J-14): as J3.4's test 9 → 404.

### Done when

- The tests pass; the generated files are committed; `just check` is green.

---

## J3.6 — The `dataset_items` and `experiment_items` SQL views

**Commit subject:** `server: dataset and experiment views in SQL`

D93 and D-J-15: the server builds both views per query, registers them with `QueryContext::register_sqlite_view` when the SQL names them, and owns their catalog in `GET /schema`.

### Files

| File | Change |
|---|---|
| `crates/server/Cargo.toml` | move `datafusion.workspace = true` from `[dev-dependencies]` to `[dependencies]` (alphabetical); no version change |
| `crates/server/src/datasets/sql_views.rs` | new |
| `crates/server/src/datasets/mod.rs` | `pub(crate) mod sql_views;` |
| `crates/server/src/meta/datasets.rs` | `view_rows` (below) |
| `crates/server/src/routes/sql.rs` | register in `run_json` and `start_export` |
| `crates/server/src/mcp/sql.rs` | register in `query_sql` |
| `crates/server/src/api/convert.rs` | `schema_response` appends the server views |
| `crates/storage/src/views/catalog.rs` | its fixture test skips the server-owned views (one test edit) |
| `fixtures/api/static/schema.json` | the two views added (hand-edited) |
| `crates/server/tests/snapshots/schema_response.snap` | unchanged (check) |

### Read first

- `crates/storage/src/views/mod.rs`: `SqliteView` (`ExperimentItems`, `DatasetItems`), `register_sqlite_view`, `TimeWindow::{from, to}`.
- `crates/storage/src/views/catalog.rs`: `ColumnInfo` (public fields), `ColumnKind`, `ViewColumns`, and the test `catalog_matches_the_schema_fixture`.
- `crates/storage/src/test_support.rs` `mem_table` (how a `MemTable` is built: `MemTable::try_new(schema, vec![vec![batch]])`).
- `crates/server/src/routes/sql.rs` `run_json`, `start_export`, `check_request` (its `checked.query`, `checked.window`); `crates/server/src/mcp/sql.rs` `query_sql`.
- `crates/server/src/api/convert.rs` `schema_response`, `column_type`; `crates/server/src/api/schema.rs` `ViewName` (it already has `ExperimentItems` and `DatasetItems`).
- `crates/server/src/meta/experiments.rs` `view_rows` (J3.1).

### `meta::datasets::view_rows`

Add to M1's store:

```rust
/// One live item of a dataset at its `current_version`, for the `dataset_items` SQL view.
/// `Debug` by hand: ids and text lengths only (design § Conventions, Privacy).
#[derive(Clone, PartialEq)]
pub(crate) struct ViewItem {
    pub(crate) dataset_id: Uuid,
    pub(crate) dataset_name: String,
    pub(crate) dataset_version: i64,
    pub(crate) item_id: Uuid,
    pub(crate) input: String,
    pub(crate) expected_output: Option<String>,
    pub(crate) metadata: String,
    pub(crate) source_trace_id: Option<TraceId>,
    pub(crate) source_span_id: Option<SpanId>,
    pub(crate) created_at: i64,
}

/// What `view_rows` returns.
pub(crate) enum ViewItems {
    Rows(Vec<ViewItem>),
    /// More than `max_rows` rows or more than `max_text_bytes` of text (D-J-15).
    TooLarge,
}

/// The live items of every dataset of `project` at its current version, ordered by dataset id
/// then item id. Rows are streamed (`fetch`); reading stops with `TooLarge` as soon as the rows
/// exceed `max_rows` or the summed byte lengths of `input`, `expected_output` and `metadata`
/// exceed `max_text_bytes`.
pub(crate) async fn view_rows(
    pool: &SqlitePool,
    project: ProjectId,
    max_rows: usize,
    max_text_bytes: usize,
) -> Result<ViewItems, MetaError>;
```

SQL: `SELECT d.id, d.name, d.current_version, i.item_id, i.input, i.expected_output, i.metadata, i.source_trace_id, i.source_span_id, i.created_at FROM dataset_items i JOIN datasets d ON d.id = i.dataset_id WHERE d.project_id = ? AND i.valid_from_version <= d.current_version AND (i.valid_to_version IS NULL OR i.valid_to_version > d.current_version) ORDER BY d.id, i.item_id` (no `LIMIT`: the stream stops itself). Tests in `meta/datasets.rs`: `view_rows_are_the_current_versions`: two datasets, one item edited (version 2) and one deleted; the rows hold the edited revision only once, not the deleted item. `view_rows_stop_at_the_budget`: three items; `max_rows 2` → `TooLarge`; `max_text_bytes` one less than the three items' summed text lengths → `TooLarge`; equal → `Rows` of 3.

### `datasets/sql_views.rs`

```rust
//! The `dataset_items` and `experiment_items` SQL views (D93, D-J-15).

/// Most rows one view holds (D93).
pub(crate) const MAX_VIEW_ROWS: usize = 1_000_000;
/// Most text one view holds, summed over its JSON and error text columns (D-J-15).
pub(crate) const MAX_VIEW_TEXT_BYTES: usize = 256 * 1024 * 1024;

/// Registers each view `sql` names (a case-insensitive substring test) on `ctx`.
pub(crate) async fn register(
    state: &AppState,
    ctx: &QueryContext,
    org: OrgId,
    project: ProjectId,
    sql: &str,
    window: Option<TimeWindow>,
) -> Result<(), ApiError>;

/// The catalog of both views for `GET /schema`, in this order: experiment_items, dataset_items.
pub(crate) fn catalog() -> Vec<ViewColumns>;
```

`register`:
1. `let lower = sql.to_ascii_lowercase();`
2. If `lower.contains("dataset_items")`: `meta::datasets::view_rows(reader, project, MAX_VIEW_ROWS, MAX_VIEW_TEXT_BYTES)`; `TooLarge` → `ApiError::Sql(RunError::Failed("dataset_items is too large for the SQL console".to_owned()))` (`query_failed`, 400); build the batch; `ctx.register_sqlite_view(SqliteView::DatasetItems, table).map_err(ApiError::Sql)?`.
3. If `lower.contains("experiment_items")`: `meta::experiments::view_rows(reader, project, crate::experiments::visible_from(state, org), window.map(|w| (w.from(), w.to())), MAX_VIEW_ROWS, MAX_VIEW_TEXT_BYTES)`; `TooLarge` → the same error with `experiment_items is too large for the SQL console`; build; register `SqliteView::ExperimentItems`.

Schemas (`arrow::datatypes::Schema`, all `Field::new(name, type, nullable)`), in this order, with `ts = DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into()))`:

| `dataset_items` | type | null |
|---|---|---|
| `dataset_id` | Utf8 | no |
| `dataset_name` | Utf8 | no |
| `dataset_version` | Int64 | no |
| `item_id` | Utf8 | no |
| `input` | LargeUtf8 (JSON text) | no |
| `expected_output` | LargeUtf8 (JSON text) | yes |
| `metadata` | LargeUtf8 (JSON text) | no |
| `source_trace_id` | Utf8 (hex) | yes |
| `source_span_id` | Utf8 (hex) | yes |
| `created_at` | ts | no |

| `experiment_items` | type | null |
|---|---|---|
| `experiment_id` | Utf8 | no |
| `experiment_name` | Utf8 | no |
| `dataset_id` | Utf8 | yes |
| `dataset_version` | Int64 | no |
| `dataset_item_id` | Utf8 | no |
| `repetition` | Int64 | no |
| `trace_id` | Utf8 (hex) | yes |
| `input` | LargeUtf8 (JSON text) | no |
| `expected_output` | LargeUtf8 (JSON text) | yes |
| `output` | LargeUtf8 (JSON text) | yes |
| `error` | LargeUtf8 | yes |
| `latency_ns` | Int64 | yes |
| `created_at` | ts | no |

The text columns (`input`, `expected_output`, `metadata`, `output`, `error`) are `LargeUtf8` (`LargeStringArray`, 64-bit offsets): with `StringArray`'s 32-bit offsets, a column over 2 GiB would panic, and AGENTS.md forbids panics on stored data; the 256 MiB budget keeps them far below anyway. Ids, names and hex ids stay `Utf8`. Build arrays with `StringArray::from(Vec<Option<String>>)` / `LargeStringArray::from(Vec<Option<String>>)` / `Int64Array::from(Vec<…>)` / `TimestampNanosecondArray::from(Vec<i64>).with_timezone("UTC")`, then `RecordBatch::try_new` and `MemTable::try_new(schema, vec![vec![batch]])` (`datafusion::datasource::memory::MemTable`). Errors from arrow or DataFusion are `ApiError::Internal(Box::new(error))`. An empty view is a batch with zero rows (a query naming it answers no rows, not "unknown table").

`catalog()` builds two `ViewColumns` with `ColumnInfo { name, kind, nullable, description }` literals (the fields are public): `view: ViewName::ExperimentItems` with description `"One row per experiment result (item × repetition) of the project's visible experiments."`, and `ViewName::DatasetItems` with `"Live items of each dataset at its current version."`. Kinds: Utf8 ids and text → `String`; the JSON text columns (`input`, `expected_output`, `metadata`, `output`) → `Json`; Int64 → `Integer`; ts → `Timestamp`. Descriptions, one short sentence each, e.g. `"Dataset item id, stable across versions."`, `"Task output, JSON text."`, `"Trace of the task run, lowercase hex."`, `"Task run time in nanoseconds, as the SDK measured it."`, `"Time the result was stored."` (write one per column; they become the fixture text).

### Registration

- `routes/sql.rs` `run_json`: after `open_query_windowed(…)`, `crate::datasets::sql_views::register(state, &ctx, access.project.org_id, access.project.id, &checked.query, checked.window).await?;`. Same in `start_export`, before `sql_export`.
- `mcp/sql.rs` `query_sql`: after `open_query_windowed`, `register(state, &ctx, …, &args.query, Some(window)).await?;`.

The sandbox accepts a registered name as a table (it reads `registered_view_names`); nothing else changes in storage.

### Catalog

`convert::schema_response()`: `views` = storage's `view_catalog()` followed by `crate::datasets::sql_views::catalog()`, mapped the same way. Then hand-edit `fixtures/api/static/schema.json`: append the two view objects after `scores`, with the exact names, column types (`string`, `integer`, `timestamp`, `json`), `nullable` values and descriptions of `catalog()`. `cargo test -p llmobs-server schema_is_the_static_fixture` must pass; `just api-examples` checks the same file.

`crates/storage/src/views/catalog.rs` test `catalog_matches_the_schema_fixture`: before comparing, keep only the fixture views whose `name` is not `experiment_items` or `dataset_items` (a one-line `filter` with a comment: "The server owns these two views' catalog (D93, D-J-15)."). No other storage change.

`web/` reads the fixture in two tests (`components/sql/sql-editor.test.tsx`, `lib/sql/examples.test.ts`); run `pnpm --dir web test -- sql` and fix nothing unless they fail; if they fail, STOP and report.

### Tests (`datasets/sql_views.rs`, through `POST …/sql` with the seeded cookie; use the helpers `loaded`/`post_sql` of `routes/sql.rs` tests if they are `pub(super)`, else write local ones)

1. `dataset_items_lists_current_items`: a dataset with 2 items, one edited → `SELECT item_id, input FROM dataset_items ORDER BY item_id` returns 2 rows, the edited input.
2. `experiment_items_lists_results`: one experiment with 2 results → `SELECT experiment_name, repetition FROM experiment_items` returns 2 rows with the name.
3. `window_applies_to_experiment_items`: results at two times (clock advanced between them); `from`/`to` around the second → one row; `dataset_items` with the same window keeps both items.
4. `hidden_experiments_are_left_out` (D-J-14): set `data_access_days` 30, create, advance 31 days, create another; the view has only the second's rows.
5. `views_are_built_only_when_named`: a query on `traces` only does not call `view_rows` (assert by deleting nothing and counting: insert a dataset row whose `input` is not valid JSON text, e.g. `x`, through SQL; a `traces` query still succeeds, and a `dataset_items` query also succeeds, since text is passed through). Name it plainly and keep it simple: the point is that a query not naming the view never reads `meta.db`'s items.
6. `other_projects_are_not_visible`: a dataset in project B is absent from project A's view.
7. `mcp_query_sql_sees_the_views`: through the MCP test harness of `mcp/sql.rs` (`app_with_token`, `call`), `SELECT count(*) AS n FROM dataset_items` answers the count.
7b. `too_large_is_query_failed`: `register` calls a private `register_with(state, ctx, org, project, sql, window, max_rows, max_text_bytes)` with `MAX_VIEW_ROWS` and `MAX_VIEW_TEXT_BYTES`; the test calls `register_with` with `max_rows 1` on a project with two dataset items and `SELECT * FROM dataset_items` → `ApiError::Sql(RunError::Failed(message))` with `message` naming `dataset_items`; the same with `max_text_bytes 1` on `experiment_items`.
8. `schema_lists_the_server_views`: `GET …/schema` contains `experiment_items` and `dataset_items` with their columns (covered by `schema_is_the_static_fixture`; add an explicit assertion on the two names).

### Done when

- The tests pass, the storage catalog test passes, the web SQL tests pass, `just check` is green.

---

## J3.7 — The deletion cascade, and closeout

**Commit subject:** `server: delete dataset and experiment copies of deleted traces`

D-J-9 (D24, D-M-14). One statement block in the deletion's scope write.

Note (from J3.1): the `Cascade` struct (`dataset_item_revisions`, `experiment_items` counts) was left out of `meta/experiments.rs` in J3.1, because nothing used it there and an unused type fails clippy's test build. Add it here, with `delete_copies_of_tx`, which returns it.

### Files

| File | Change |
|---|---|
| `crates/server/src/meta/experiments.rs` | `delete_copies_of_tx` |
| `crates/server/src/deletion/resolve.rs` | call it in the scope write |
| `crates/server/src/deletion/mod.rs` | tests |
| `CHANGELOG.md` | § `[Unreleased]` → `Added` |
| `docs/roadmap.md` | J's row: M3 merged |
| `docs/plans/j-datasets/00-design.md` | `Status:` line |

### Read first

- `crates/server/src/deletion/resolve.rs` `resolve` (the `state.meta.write` that calls `insert_traces`, `insert_sessions`, `store_counts` and `eval_jobs::delete_jobs`).
- `crates/server/migrations/meta/0007_deletions.sql` `deletion_traces (deletion_id, trace_id BLOB, PRIMARY KEY (deletion_id, trace_id))`; `meta/deletions.rs` `insert_traces` (it stores `trace.as_bytes()`, the same 16 bytes J stores).
- `crates/server/src/deletion/mod.rs` tests: `commit`, `new_row`, `insert`, `worker`, `count_rows`, `trace_ids_are_hidden_once_registered`, `user_resolves_traces_and_sessions`, `project_drop_removes_the_project`.

### `meta::experiments::delete_copies_of_tx`

```rust
/// D-J-9: deletes, in `project`, the experiment results whose trace is in the deletion's scope,
/// the results whose snapshot came from a dataset item copied from a trace in the scope, and
/// those dataset item revisions (all versions). Reads the scope from `deletion_traces`, inside
/// the caller's write transaction, after `insert_traces` stored it.
pub(crate) async fn delete_copies_of_tx(
    conn: &mut SqliteConnection,
    deletion: Uuid,
    project: ProjectId,
) -> Result<Cascade, MetaError>;
```

Three statements, in this order (the first needs the revisions the third deletes):

```sql
DELETE FROM experiment_items
WHERE experiment_id IN (SELECT id FROM experiments WHERE project_id = ?1)
  AND revision_id IN (
    SELECT i.revision_id FROM dataset_items i JOIN datasets d ON d.id = i.dataset_id
    WHERE d.project_id = ?1
      AND i.source_trace_id IN (SELECT trace_id FROM deletion_traces WHERE deletion_id = ?2));

DELETE FROM experiment_items
WHERE experiment_id IN (SELECT id FROM experiments WHERE project_id = ?1)
  AND trace_id IN (SELECT trace_id FROM deletion_traces WHERE deletion_id = ?2);

DELETE FROM dataset_items
WHERE dataset_id IN (SELECT id FROM datasets WHERE project_id = ?1)
  AND source_trace_id IN (SELECT trace_id FROM deletion_traces WHERE deletion_id = ?2);
```

These are set-based statements over the scope rows (`deletion_traces`) that the same transaction has just written (D-J-9); nothing is read into memory or chunked. Bind `project.to_string()` and `deletion.to_string()` (sqlx binds `?1`/`?2` by position with numbered placeholders; if sqlx's SQLite driver in this workspace does not accept `?1`, bind the values once per `?` in order). `Cascade { experiment_items: first + second rows_affected, dataset_item_revisions: third }`. The indexes `dataset_items_source`, `experiment_items_trace` and `experiment_items_revision` (M1's migration) serve the lookups. The datasets' `current_version` and `dataset_versions` are not changed: a version's item set simply loses the erased items.

### `resolve.rs`

Inside the existing write closure, **in the `if stored { … }` block** after `delete_jobs` (the scope exists only when it was stored; outside that block `deletion_traces` may be empty or stale):

```rust
let cascade = crate::meta::experiments::delete_copies_of_tx(conn, id, project).await?;
```

Return it with `stored` (change the closure's result to `(bool, Cascade)`), and after the write log `tracing::info!(target: "audit", deletion_id = %row.id, project_id = %project, dataset_item_revisions = cascade.dataset_item_revisions, experiment_items = cascade.experiment_items, "deletion removed dataset and experiment copies")` when either count is non-zero. `project` must be in scope inside the closure (`let project_id = project;` before it, moved in). Update the comment above the write: "J's cascade (D-J-9) reads the scope from `deletion_traces` in this transaction, so an item added from a trace before it is never missed; add-from-trace checks the guard in its own write, so one added after the guard is filled is refused."

### Tests (`deletion/mod.rs`)

Helper `async fn copy_of(app, project, trace: TraceId) -> (Uuid /*item*/, Uuid /*experiment*/)`: creates a dataset with one item whose `source_trace_id` is `trace` (M1's `insert_dataset` and `write_version` with a `NewRevision` carrying the source ids; M2's add-from-trace route is not needed), a second item without a source, an experiment pinning version 1, and two results: item 1 with trace `trace`, item 2 with an unrelated trace `u`.

1. `trace_deletion_erases_copies`: `copy_of(trace(1))`; a `trace_ids` deletion of `trace(1)`; `worker`. Every revision of item 1 is gone (`count_rows(&app, "SELECT count(*) FROM dataset_items WHERE item_id = ?", item.to_string())` is 0: `count_rows` takes `(app, &'static str, String)`); item 2 remains; both results of item 1 and the result on trace 1 are gone; the result of item 2 on `u` remains.
2. `edited_copies_go_too`: edit item 1 before the deletion with M1's `write_version` (close it, insert a new revision with the same source ids, as M1's PATCH route does) → both revisions are gone.
3. `user_deletion_erases_copies`: spans of `trace(2)` with `user_id = "u-1"`, a copy of `trace(2)`; a `user` deletion → the copy is gone.
4. `other_projects_are_untouched`: the same trace id copied into a dataset of project B; a deletion in project A leaves B's item.
5. `nothing_to_cascade_logs_nothing`: a deletion of a trace with no copy writes no `deletion removed dataset and experiment copies` line (capture logs, `#[tokio::test]`).
6. `project_drop_removes_everything`: after `project_drop_removes_the_project`'s flow on a project with a dataset and an experiment, `datasets`, `dataset_items`, `experiments` and `experiment_items` hold no row of the project (FK cascade).

### Closeout

- `CHANGELOG.md` § `[Unreleased]` → `### Added`: "Experiments API: create an experiment from a dataset version, store results and finish it; results with score summaries and a 2–4 experiment comparison; experiment scores; `dataset_items` and `experiment_items` in the SQL console and MCP; deleting a trace also deletes dataset items copied from it and experiment results about it (track J)."
- `docs/roadmap.md` J row, Status: append "M3 (experiments API, results and compare, SQL views, deletion cascade) merged."
- `00-design.md` `Status:` line: `M3 done`.

### Done when

- The tests pass; the existing deletion tests pass unchanged; `just check` is green.

---

## Design changes found while writing this plan

These are reported to the coordinator of the J plan; the plan above already follows them.

1. `CreateExperimentResponse` holds `items: Vec<DatasetItem>` and `next_cursor` flat instead of `items: Page<DatasetItem>` (no OpenAPI name for a nested generic page).
2. The per-row score type is `ResultScore`, not `ScoreValue` (name clash with `llmobs_core::score::ScoreValue`).
3. The derived score id of an experiment score uses `experiment_id` alone as its author (spec 06 said `experiment_id + dataset_item_id`; the trace already identifies the item and repetition). The plan PR amended spec 06; J3.2 checks it.
4. `crates/server` needs `datafusion` as a normal dependency (it was a dev-dependency) to build the views' `MemTable`s; no version change.
5. `fixtures/api/static/schema.json` gains the server views, so storage's `catalog_matches_the_schema_fixture` must skip them (one test line in `crates/storage`).
6. `ScoreCreate` literals across the server (about 40, mostly tests) gain two `None` fields.
7. `meta::experiments::count_version_items` counts a pinned (possibly older) version's live items; M1's `DatasetRow.item_count` covers only the current version.
8. The experiment fixtures pin `faq` at version 2 (M1 leaves it there), so the snapshot revisions are `…0d11`, `…0d14`, `…0d13`.
9. After the cold review (all now in the design): the guard on results is checked inside the write; create is one transaction; a repeated finish with the same outcome is 200; outputs are capped at 32 MiB per request (40 MiB body); results and compare aggregate from a light projection and read text per page; deltas count only items both ran without error (`{delta, n_compared}`); cells exist only for experiments with a result; `DELETE` ignores the window; the SQL views have a 256 MiB text budget and `LargeUtf8` text columns; the storage reads take a lower time bound (`created_at − CLOCK_SKEW_NS`).
