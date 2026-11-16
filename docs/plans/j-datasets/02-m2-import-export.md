# J · M2 — Import, export and add-from-trace

Implementation plan for milestone M2 of track J. It has 4 sub-phases, **J2.1 to J2.4**, done in order by one agent in one worktree. **Haiku** implements each sub-phase in one go, with no context beyond this plan, the design doc and the files they name. Each sub-phase is one commit and ends with `just check` green. Sonnet reviews the milestone after J2.4.

M2 adds three things to the datasets of M1 (spec 06 § Datasets, spec 09 § Datasets & experiments, D-J-6, D-J-7, D-J-8):

- CSV and JSONL import: a preview of the head of the file, then the import of the whole file as one version;
- export of a version as JSONL or CSV, streamed;
- "Add to dataset" from a trace or a span: one item copied from the trace, made a new version.

It adds no table and no migration. The web pages that call these routes are M5.

## Prerequisites (check before you start)

- M1 is merged on `main`. These must each print at least one line, otherwise STOP and report:

```bash
grep -n "ManageDatasets" crates/server/src/auth/roles.rs
```

```bash
grep -n "pub(crate) fn routes" crates/server/src/routes/datasets.rs
```

```bash
ls crates/server/src/meta/datasets.rs crates/server/src/api/datasets.rs crates/server/src/datasets/mod.rs
```

- Read `docs/plans/j-datasets/01-m1-datasets.md` § the store, § validation and § routes, and note the real names of the M1 functions this plan uses (the table in § M1 names below). This plan writes them as the design implies them; **when M1 named one differently, use M1's name** — the arguments and the behaviour are what matter. If an M1 function this plan needs does not exist at all, STOP and report.
- Node `26.13.0` is installed at `$HOME/.local/share/nvm/v26.13.0/bin` (not on the non-interactive `PATH`).

## Read first (every sub-phase)

- `AGENTS.md`, all of it.
- `docs/plans/j-datasets/00-design.md`: § Decisions (D-J-2 to D-J-9 and D-J-18), § API contract, § Parallel work, § Conventions.
- This file up to § J2.1, then your own sub-phase.

## M1 names

The functions and types M1 defined that M2 calls (M1 § Interfaces for M2 and M3). Use exactly these; if one is missing or has another signature in the code, STOP and report.

| Name | Where | Use in M2 |
|---|---|---|
| `find(&state, project, raw) -> Result<DatasetRow, ApiError>` | `routes::datasets` | resolve `{dataset}` in every M2 handler (404 when absent) |
| `DatasetKey::parse`, `get_dataset_tx(conn, project, &key)` | `meta::datasets` | re-read the dataset inside a write transaction (schema re-check, `None` → 404) |
| `DatasetRow` (`id`, `project_id`, `current_version`, `input_schema`, `expected_schema`, …) | `meta::datasets` | the stored dataset |
| `NewRevision { revision_id, item_id, input, expected_output, metadata, source_trace_id, source_span_id }`, `VersionChange { close, insert, description, created_by }`, `write_version(conn, dataset_id, change, now) -> Result<i64, MetaError>` | `meta::datasets` | every M2 write: an import is `close: vec![]` and one `NewRevision` per valid row; add-from-trace is one `NewRevision` with the source ids. Both ids are `Uuid::now_v7()`. The 50,000 live-item cap is enforced inside (`MetaError::Conflict`, 409). No store change in M2 |
| `list_items(pool, dataset_id, version, after, limit)` | `meta::datasets` | export pages (`limit + 1` rule) |
| `ItemValues { input, expected_output, metadata }`, `Schemas::of(&DatasetRow)`, `check_item(prefix, &ItemValues, &Schemas) -> Vec<FieldError>` | `crate::datasets` | compile the schemas once per request with `Schemas::of`, then `check_item("", …)` per row |
| `created_by(&principal)`, `version_of(&row, requested)`, `item_json(&ItemRow)`, `DatasetPath` | `routes::datasets` | version descriptions, `?version=`, the API item, the path extractor |
| `crate::api::common::any_json` | `api::common` | the OpenAPI schema of every any-JSON field: `#[schema(schema_with = any_json)]` (never `value_type = Object`, which only fits JSON objects) |
| tag `datasets`, `dataset_paths`, `path_string("dataset", …)` | `api::openapi` | where M2's operations go |
| the access suite's `{dataset}` → `faq` replacement | `http/access_suite.rs` | M2's routes need no new replacement |
| `TestApp::seed_example_dataset`, `TestApp::write_key`, `EXAMPLE_DATASET` | `test_support`, `api_examples.rs` | fixtures and tests |

**Row messages.** An import row's message is built from the first `FieldError` that `check_item("", …)` returns: `format!("{}: {}", error.field, error.message)` (for example `input: /: required`, `item: is larger than 1 MiB`). There is no separate validator type.

**Schema re-check.** Import and add-from-trace validate outside the write transaction (on `spawn_blocking`, or before the write). Inside their `state.meta.write` closure they first call `get_dataset_tx`; when its `input_schema` or `expected_schema` text differs from the `DatasetRow` used for validation, the closure returns without writing and the handler answers `ApiError::Conflict { detail: "the dataset's schema changed; retry" }` (409, D-J-2).

## Conventions for every sub-phase

- **Privacy.** Logs carry the dataset id, project id, user or key id, version, row and byte counts, and the format. Never a file name, a column name, a mapping target, a cell or any item content (AGENTS.md § Privacy). Error messages returned to the caller may name a column or a key: they go to the person who sent them, not to logs.
- **Errors.** problem+json through `crate::http::error::ApiError`. Field errors use `FieldError` (`crate::api::common`) with the JSON path as `field`.
- **No `unwrap`/`expect`** outside tests unless a comment states the invariant.
- **Tests.** Unit tests in the module they test (`#[cfg(test)] mod tests`). HTTP tests use `TestApp` (`crates/server/src/test_support/mod.rs`): `TestApp::new()`, `app.seed()`, `app.request(Method, path).header("cookie", &seeded.cookie)…send()`. `#[tokio::test(flavor = "multi_thread")]`, except tests that capture logs (`capture_logs`, `#[tokio::test]`).
- **API contract changes.** Run `just openapi` and commit `crates/server/openapi.json` and `web/src/api/schema.gen.ts`. Fixture changes: `just api-examples`, commit the generated files.
- **Commands.**

```bash
export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH
```

```bash
just check
```

Focused runs: `cargo test -p cetra-server datasets`, `cargo test -p cetra-server --test openapi`, `cargo test -p cetra-server --test api_examples`.

## Parallel work

M2 can run while M3 runs in another worktree (design § Parallel work). The files both touch are additive: `crates/server/src/api/openapi.rs`, `crates/server/tests/openapi.rs`, `crates/server/openapi.json`, `web/src/api/schema.gen.ts`, `crates/server/src/api_examples.rs`, `crates/server/tests/api_examples.rs`, `fixtures/api/manifest.json`, `CHANGELOG.md`. After a rebase, regenerate the generated files (`just openapi`, `just api-examples`); never hand-merge them. M2 alone touches the root `Cargo.toml` in J (N's M3 may too: additive lines only).

## Final layout after M2

```
Cargo.toml                                   + csv, axum "multipart"                    (J2.1)
crates/server/Cargo.toml                     + csv                                      (J2.1)
crates/server/src/datasets/import.rs         CSV/JSONL parsing, mapping, preview        (J2.1)
crates/server/src/datasets/mod.rs            + pub(crate) mod import                    (J2.1)
crates/server/src/api/datasets.rs            + ImportPreview, ImportMapping, ImportColumn,
                                               ImportResult, ImportRowError,
                                               ImportFormat, ExportFormat,
                                               FromTraceRequest, …Params                (J2.2–J2.4)
crates/server/src/routes/datasets.rs         + preview, import, export, from-trace      (J2.2–J2.4)
crates/eval/src/mapping/resolve.rs           + TraceData::root_span_id                  (J2.4)
crates/server/src/api/openapi.rs, tests/openapi.rs, openapi.json, schema.gen.ts         (J2.2–J2.4)
fixtures/api/static/dataset-import-preview.json, dataset-import-result.json,
  dataset-item-from-trace.json; fixtures/api/requests/dataset-import-mapping.json,
  dataset-from-trace.json                                                                (J2.2, J2.4)
```

---

## J2.1 — Parsing: CSV and JSONL rows to items

**Commit subject:** `server: parse dataset imports from CSV and JSONL`

Adds the dependencies and a pure parsing module: no route, no database. J2.2 calls it.

### Read first

- Design D-J-2, D-J-4, D-J-6.
- `crates/server/src/datasets/mod.rs` and `crates/server/src/api/datasets.rs` (M1).
- The root `Cargo.toml` `[workspace.dependencies]` (the comment style above each entry; `axum`'s feature list) and `crates/server/Cargo.toml` `[dependencies]`.
- `crates/server/src/api/common.rs` (`FieldError`).

### Files

| File | Change |
|---|---|
| `Cargo.toml` | `csv = "1.4.0"` in `[workspace.dependencies]` with a comment; `"multipart"` added to axum's feature list |
| `crates/server/Cargo.toml` | `csv = { workspace = true }` (alphabetical) |
| `Cargo.lock` | updated by cargo |
| `crates/server/src/datasets/mod.rs` | `pub(crate) mod import;` |
| `crates/server/src/datasets/import.rs` | new |
| `crates/server/src/api/datasets.rs` | `ImportFormat`, `ImportMapping`, `ImportColumn`, `ImportRowError` |

### 1. Dependencies

Check the versions first:

```bash
cargo info csv
```

It must show `version: 1.4.0` and licence `Unlicense/MIT`. Add to `[workspace.dependencies]`, next to the other parsing crates (alphabetical among its neighbours):

```toml
# Dataset CSV import and export (track J, D-J-6). `csv` is already in the lock as a dependency of arrow-csv.
csv = "1.4.0"
```

Add `"multipart",` to axum's feature list (keep the list alphabetical: after `"matched-path"`). Do not change any version. Then `crates/server/Cargo.toml`: `csv = { workspace = true }`.

`cargo deny check` (part of `just check`) must stay green. `multer` (axum's multipart parser) comes in with the feature; if `cargo deny` refuses its licence or a duplicate version, STOP and report.

### 2. API types (`api/datasets.rs`)

Add, with doc comments on every item and field (house style of the file):

```rust
/// The format of an import or a preview (D-J-6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ImportFormat {
    /// Comma-separated values with a header record (RFC 4180).
    Csv,
    /// One JSON object per line.
    Jsonl,
}

/// Where one CSV column goes (D-J-6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportColumn {
    /// The header of the column, exactly as in the file.
    pub column: String,
    /// `input`, `input.<key>`, `expected`, `expected.<key>`, `metadata.<key>` or `ignore`.
    pub target: String,
}

/// The column mapping of a CSV import. Columns the mapping does not list are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportMapping {
    /// One entry per mapped column, at most 1,000.
    pub columns: Vec<ImportColumn>,
}

/// One rejected row of an import or a preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ImportRowError {
    /// CSV: the 1-based data record (the header is not counted). JSONL: the 1-based line.
    pub row: i64,
    /// Why the row was rejected.
    pub message: String,
}
```

### 3. `datasets/import.rs`

Module doc: `//! Parses dataset imports: CSV with a column mapping and JSONL (D-J-6). Pure functions: no database, no I/O.`

Right after it, add (J2.2 removes the line):

```rust
// Used by tests only until J2.2 adds the routes; J2.2 removes this line.
#![cfg_attr(not(test), allow(dead_code))]
```

Imports: `serde_json::{Map, Value}`, `crate::api::datasets::{ImportColumn, ImportFormat, ImportMapping, ImportRowError}`, `crate::api::common::FieldError`, `crate::datasets::ItemValues`.

#### Constants (exactly these)

```rust
/// The largest import file (D-J-6).
pub(crate) const MAX_FILE_BYTES: usize = 100 * 1024 * 1024;
/// The most data rows (CSV records after the header, JSONL non-empty lines) in one import.
pub(crate) const MAX_ROWS: usize = 50_000;
/// The largest preview body (D-J-6).
pub(crate) const MAX_PREVIEW_BYTES: usize = 1024 * 1024;
/// Rows a preview shows.
pub(crate) const PREVIEW_ROWS: usize = 20;
/// Row errors listed in an answer; `error_count` counts all of them.
pub(crate) const MAX_LISTED_ERRORS: usize = 100;
/// Mapped columns at most.
pub(crate) const MAX_COLUMNS: usize = 1_000;
/// The longest key of `input.<key>`, `expected.<key>` or `metadata.<key>`, in characters.
pub(crate) const MAX_KEY_CHARS: usize = 200;
```

#### Types

```rust
/// Where a column goes, parsed from `ImportColumn::target`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    Input,
    InputKey(String),
    Expected,
    ExpectedKey(String),
    MetadataKey(String),
    Ignore,
}

/// A checked mapping: for each header index, its target (`Ignore` for unlisted columns).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    targets: Vec<Target>,
}

/// The parsed rows of a whole file: valid items with their row number, and the row errors.
#[derive(Debug, Default)]
pub(crate) struct Parsed {
    /// Valid rows in file order: `(row, item)`.
    pub(crate) items: Vec<(i64, ItemValues)>,
    /// Every row error, in file order (the caller lists the first `MAX_LISTED_ERRORS`).
    pub(crate) errors: Vec<ImportRowError>,
}

/// A failure of the whole file (a 400), not of one row.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum FileError {
    /// The mapping is invalid. Each entry is a field error with `field = "mapping.columns[<i>]…"`.
    #[error("invalid mapping")]
    Mapping(Vec<FieldError>),
    /// The CSV header is missing, empty, not UTF-8, or has empty or duplicate names.
    #[error("{0}")]
    Header(String),
    /// More than `MAX_ROWS` data rows.
    #[error("the file has more than 50000 rows")]
    TooManyRows,
}
```

`Parsed` holds item content, so implement `Debug` by hand like `LoadedTrace` in `crates/server/src/eval/trace_load.rs` (counts only) instead of deriving it; same for `Plan` (print the number of targets).

#### `parse_target`

```rust
/// Parses one target string. `Err` is the message for the field error.
pub(crate) fn parse_target(text: &str) -> Result<Target, &'static str>;
```

Rules: `"input"` → `Input`; `"expected"` → `Expected`; `"ignore"` → `Ignore`; `"input." + key`, `"expected." + key`, `"metadata." + key` → the keyed variants, where `key` is everything after the first dot (it may contain more dots: no nesting), 1–`MAX_KEY_CHARS` characters (`chars().count()`), not all whitespace. Anything else, including `"metadata"` alone and an empty key, is `Err("must be input, input.<key>, expected, expected.<key>, metadata.<key> or ignore")`. A key that is too long is `Err("the key is longer than 200 characters")`.

#### `plan`

```rust
/// Checks `mapping` against the CSV header and returns the target of every column.
pub(crate) fn plan(header: &[String], mapping: &ImportMapping) -> Result<Plan, FileError>;
```

Collect every error, then return `FileError::Mapping(errors)` when there is at least one. Checks, in this order for each entry `i`:

1. `mapping.columns.len() > MAX_COLUMNS` → one error `field: "mapping.columns"`, message `at most 1000 columns`, and return at once.
2. The column is not in the header → `mapping.columns[i].column`: `no such column`.
3. The column was already listed by an earlier entry → `mapping.columns[i].column`: `listed twice`.
4. `parse_target` fails → `mapping.columns[i].target`: its message.
5. The same non-`Ignore` target as an earlier entry (same variant and key) → `mapping.columns[i].target`: `another column has this target`.

After the loop:

6. Both `Input` and some `InputKey` appear → `mapping.columns`: `input cannot be both whole and keyed`. The same for `Expected`/`ExpectedKey`: `expected cannot be both whole and keyed`.
7. Neither `Input` nor any `InputKey` → `mapping.columns`: `no column maps to input`.

`position` of every `FieldError` is `None`. Header columns no entry lists get `Target::Ignore`.

#### Cell values

```rust
/// A CSV cell as a JSON value: a cell whose trimmed text starts with `{` or `[` and parses as
/// JSON becomes that value; any other non-empty cell is the cell text as a string (untrimmed).
/// An empty cell (length 0) is `None`: its key is left out.
fn cell_value(cell: &str) -> Option<Value>;
```

So `"42"` stays the string `"42"`, `" {\"a\":1} "` becomes `{"a":1}`, `"{not json"` stays a string, `""` is `None`, `" "` is the string `" "`.

#### `parse_csv`

```rust
/// Parses a whole CSV file with a mapping. `bytes` is the file as uploaded.
pub(crate) fn parse_csv(bytes: &[u8], mapping: &ImportMapping) -> Result<Parsed, FileError>;
```

Steps:

1. Strip one leading UTF-8 BOM (`EF BB BF`) if present.
2. `csv::ReaderBuilder::new().has_headers(false).flexible(true).from_reader(bytes)`. Read with `byte_records()`, so a non-UTF-8 record is a row error, not a file error.
3. The first record is the header. None (empty file) → `FileError::Header("the file has no header")`. Not UTF-8 → `Header("the header is not valid UTF-8")`. An empty name (after trimming) → `Header("column <n> has an empty name")` (1-based `n`). A name equal to an earlier one → `Header("column <n> repeats the name of column <m>")`. A read error on the header (`csv::Error`) → `Header("the header cannot be read")`.
4. `plan(&header, mapping)?`.
5. For each following record, `row` counts from 1 (a quoted field with newlines is one record, so `row` is the record number, not the line number). When `row` exceeds `MAX_ROWS`, return `FileError::TooManyRows` at once (do not parse the rest). Per record:
   - a `csv::Error` from the reader → row error `the row cannot be read`, continue;
   - `record.len() != header.len()` → `the row has <len> fields, the header has <n>`;
   - a field that is not UTF-8 → `the row is not valid UTF-8`;
   - build the item: `Input` sets `input = cell_value(cell)`; each `InputKey(k)` inserts `k → value` into an input object; the same for `Expected`/`ExpectedKey` and `expected_output`; `MetadataKey(k)` inserts into `metadata`. A `None` cell value is skipped. A keyed field with no key set is absent (for input: no input). `metadata` is always a `Map`, possibly empty;
   - no input (whole cell empty, or no keyed input cell set) → `the row has no input`;
   - otherwise push `(row, ItemValues { input, expected_output, metadata })` (`crate::datasets::ItemValues`, M1). The item checks (size, schemas) are not done here: J2.2 runs `check_item` on every parsed item.

#### `parse_jsonl`

```rust
/// Parses a whole JSONL file. Each non-empty line is one item object.
pub(crate) fn parse_jsonl(bytes: &[u8]) -> Result<Parsed, FileError>;
```

1. Strip one leading BOM.
2. Split on `b'\n'`; strip one trailing `b'\r'` from each line. `row` is the 1-based line number in the file (empty lines count as lines but are not rows).
3. Skip lines that are empty or only ASCII whitespace. Count the others; when the count exceeds `MAX_ROWS`, return `TooManyRows` at once.
4. Per line: not UTF-8 → `the line is not valid UTF-8`; `serde_json::from_str::<Value>` fails → `the line is not valid JSON`; not an object → `the line is not a JSON object`; a key other than `input`, `expected_output`, `metadata` → `unknown key <key>` (the first unknown key in the object's order); `input` absent or `null` → `the line has no input`; `metadata` present and not an object → `metadata must be an object`; `expected_output` present and `null` → absent. Otherwise push the item (`metadata` defaults to an empty object).

#### `preview`

```rust
/// What the preview shows: the columns and the first rows of the head of a file.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PreviewRows {
    pub(crate) columns: Vec<String>,
    /// CSV: each row is a JSON array of the raw cell strings. JSONL: each row is the line's value.
    pub(crate) rows: Vec<Value>,
    pub(crate) errors: Vec<ImportRowError>,
    /// More rows follow, or the head was cut (`partial`).
    pub(crate) truncated: bool,
}

/// Parses the head of a file. With `partial`, the bytes are the first part of a larger file:
/// the last record (CSV) or line (JSONL) may be cut, so it is dropped.
pub(crate) fn preview(bytes: &[u8], format: ImportFormat, partial: bool) -> Result<PreviewRows, FileError>;
```

CSV: header checks as in `parse_csv` step 3 (same `FileError::Header` messages); `columns` = the header; then up to `PREVIEW_ROWS` records as arrays of strings (a non-UTF-8 field is an `errors` entry for that row instead of a row; a record of the wrong length is shown as it is). With `partial`, read the records as usual and always drop the last record read (complete or not: a cut after a newline can still land inside a quoted multi-line cell, which the reader returns as one garbled record); when that leaves only the header, `columns` is the header and there are no rows; a header that is itself the last record read is still used as the header. `truncated` = `partial` or more records follow the 20th. The browser sends `partial=true` only when it sent less than the whole file.

JSONL: with `partial`, drop the last line (the text after the last `b'\n'`, or the last non-empty line when the head ends with a newline); then up to `PREVIEW_ROWS` non-empty lines; each line that parses as JSON is a row (any JSON, unvalidated), each one that does not is an `errors` entry (`the line is not valid JSON`, row = line number). `columns` = the distinct top-level keys of the object rows, in first-seen order. `truncated` as for CSV.

### Tests (`datasets/import.rs` `mod tests`)

Helper: `fn mapping(pairs: &[(&str, &str)]) -> ImportMapping`. Assert on `parsed.items` (row numbers and `serde_json::json!` values) and `parsed.errors` (exact `row` and `message`).

| Test | Input | Assert |
|---|---|---|
| `targets_parse` | each valid target; `metadata`, `input.`, `foo`, `output.a`, a 201-character key | the variants; the two error messages |
| `keys_may_hold_dots` | `input.a.b` | `InputKey("a.b")` |
| `csv_whole_and_keyed_columns` | header `q,a,topic`; mapping `q→input`, `a→expected`, `topic→metadata.topic`; one row `hi,there,billing` | item `input "hi"`, `expected_output "there"`, `metadata {"topic":"billing"}` |
| `csv_keyed_input` | `q,lang` → `input.q`, `input.lang`; row `hi,en` | `input {"q":"hi","lang":"en"}` |
| `csv_json_cells` | cells `{"a":1}`, ` [1,2] `, `{oops`, `42`, `true` | `{"a":1}`, `[1,2]`, `"{oops"`, `"42"`, `"true"` |
| `csv_empty_cells_drop_keys` | `input.q`, `input.c`, `metadata.t`; row `hi,,` | `input {"q":"hi"}`, `metadata {}` |
| `csv_row_without_input` | whole input column empty in row 2 | row 2 error `the row has no input`; rows 1 and 3 valid |
| `csv_quoted_newline_is_one_row` | `q,a\n"line1\nline2",x\ny,z\n` | two items, rows 1 and 2, input of row 1 is `"line1\nline2"` |
| `csv_bom_is_stripped` | `\u{feff}q,a\nhi,x\n` mapping `q→input` | no `Mapping` error, one item |
| `csv_crlf` | `q\r\nhi\r\n` | input `"hi"` |
| `csv_duplicate_header` | `q,a,q` | `Header("column 3 repeats the name of column 1")` |
| `csv_empty_header_name` | `q,,a` | `Header("column 2 has an empty name")` |
| `csv_empty_file` | `b""` | `Header("the file has no header")` |
| `csv_wrong_field_count` | header 2 columns, row with 3 | `the row has 3 fields, the header has 2` |
| `csv_invalid_utf8_row` | a row with byte `0xFF` | row error `the row is not valid UTF-8`, other rows valid |
| `mapping_errors_are_collected` | unknown column, a column twice, a bad target, two columns to `input.q`, `input` with `input.x`, no input target | every `FieldError` with the exact `field` and message |
| `csv_too_many_rows` | a header and 50,001 one-cell rows (build with `format!`/`repeat`) | `FileError::TooManyRows` |
| `csv_exactly_max_rows` | 50,000 rows | 50,000 items, no error |
| `jsonl_rows` | three lines: full item, item with `metadata`, item with `expected_output: null` | three items; the third has no `expected_output` |
| `jsonl_skips_blank_lines_and_numbers_by_line` | line 1 valid, line 2 empty, line 3 `[1]`, line 4 valid | items rows 1 and 4; error row 3 `the line is not a JSON object` |
| `jsonl_unknown_key` | `{"input":1,"id":"x"}` | `unknown key id` |
| `jsonl_missing_or_null_input` | `{"expected_output":1}`, `{"input":null}` | `the line has no input` twice |
| `jsonl_bad_json_and_metadata` | `{oops`, `{"input":1,"metadata":[1]}` | `the line is not valid JSON`, `metadata must be an object` |
| `jsonl_too_many_rows` | 50,001 lines `{"input":1}` | `TooManyRows` |
| `preview_csv_partial_drops_the_cut_record` | `q,a\nhi,x\nho,y` with `partial = true` | `columns ["q","a"]`, rows `[["hi","x"]]`, `truncated` true |
| `preview_csv_partial_drops_an_open_quote` | `q,a\nhi,x\n"open,y` with `partial = true` | rows `[["hi","x"]]`, `errors []`, `truncated` true |
| `preview_csv_partial_drops_a_complete_last_record` | `q,a\nhi,x\nho,y\n` with `partial = true` | rows `[["hi","x"]]` |
| `preview_csv_limits_rows` | header and 25 rows, `partial = false` | 20 rows, `truncated` true |
| `preview_jsonl_columns_in_first_seen_order` | `{"input":1,"metadata":{}}` then `{"expected_output":2,"input":3}` | `columns ["input","metadata","expected_output"]` |
| `preview_jsonl_partial_drops_the_last_line` | `{"input":1}\n{"input":2}\n{"inp` with `partial = true` | two rows, no error |
| `preview_jsonl_bad_line` | line 2 is `{oops` | `errors [{row: 2, message: "the line is not valid JSON"}]`, rows from lines 1 and 3 |
| `debug_prints_no_content` | `format!("{parsed:?}")` of a `Parsed` holding `"canary-import-1"` | does not contain `canary-import-1` |

### Done when

- `cargo test -p cetra-server datasets::import` passes and `just check` is green (including `cargo deny`).
- `git diff Cargo.toml` adds `csv` and `"multipart"` only.

---

## J2.2 — The preview and import routes

**Commit subject:** `server: import dataset items from CSV and JSONL`

Adds `POST …/datasets/{dataset}/import/preview` and `POST …/datasets/{dataset}/import` (D-J-6), their OpenAPI operations and fixtures.

### Read first

- J2.1's `datasets/import.rs`.
- `crates/server/src/routes/datasets.rs` (M1): the route table, `find`, `created_by`, and the `add_items` handler (how it validates with `Schemas` and `check_item`, builds `NewRevision`s, calls `write_version`, logs the audit line and answers).
- `crates/server/src/meta/datasets.rs` (M1): `get_dataset_tx`, `DatasetKey`, `NewRevision`, `VersionChange`, `write_version`; `crates/server/src/datasets/mod.rs` (M1): `ItemValues`, `Schemas::of`, `check_item`.
- `crates/server/src/routes/scores.rs` (`SCORE_BODY_LIMIT`, `post(…).layer(DefaultBodyLimit::max(…))`).
- `crates/server/src/http/error.rs` (`ApiError::{BadRequest, PayloadTooLarge, UnsupportedMediaType, Unavailable, Conflict}`, `error::Query`).
- axum 0.8's `axum::extract::Multipart`, `Field::name`, `Field::file_name`, `Field::chunk`, `MultipartError::status`, `MultipartRejection`, and `axum::extract::rejection::BytesRejection` (read the docs with `cargo doc -p axum --no-deps --open` or the source under `~/.cargo/registry`).
- `crates/server/src/api/openapi.rs`: M1's `dataset_paths`, `binary_string`, `string_schema`, `json_body`, `json_of`, `responses`; `crates/server/tests/openapi.rs`: `OPERATIONS`, the `expected_errors` chain.
- `crates/server/src/api_examples.rs` (`example_created`, the `post_evaluator` static entry), `crates/server/tests/api_examples.rs` (the static `roundtrip` list, `request_examples_match_their_types_and_the_openapi_document`, the `generated_files_roundtrip` match), `fixtures/api/README.md`.

### Files

| File | Change |
|---|---|
| `crates/server/src/api/datasets.rs` | `ImportPreview`, `ImportResult`, `PreviewParams`, `ImportParams` |
| `crates/server/src/datasets/import.rs` | remove the `dead_code` line |
| `crates/server/src/routes/datasets.rs` | two routes and handlers |
| `crates/server/src/api/openapi.rs` | schemas and two operations |
| `crates/server/tests/openapi.rs` | `OPERATIONS`, errors |
| `crates/server/openapi.json`, `web/src/api/schema.gen.ts` | regenerated |
| `fixtures/api/static/dataset-import-preview.json`, `dataset-import-result.json` | new |
| `fixtures/api/requests/dataset-import-mapping.json` | new |
| `crates/server/src/api_examples.rs`, `crates/server/tests/api_examples.rs` | examples, round-trips |
| `fixtures/api/manifest.json` | regenerated |

### 1. Types

```rust
/// `?format=&partial=` of the preview.
#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PreviewParams {
    /// The file's format.
    pub format: ImportFormat,
    /// The body is the first part of a larger file (the browser sends at most 1 MiB).
    #[serde(default)]
    pub partial: bool,
}

/// `?format=` of the import.
#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ImportParams {
    /// The file's format.
    pub format: ImportFormat,
}

/// The head of a file as the import wizard shows it (D-J-6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ImportPreview {
    pub format: ImportFormat,
    /// CSV: the header. JSONL: the top-level keys of the shown rows, in first-seen order.
    pub columns: Vec<String>,
    /// At most 20. CSV: arrays of cell strings. JSONL: the parsed lines.
    #[schema(schema_with = any_json_array)]
    pub rows: Vec<serde_json::Value>,
    /// Lines of the head that could not be read.
    pub errors: Vec<ImportRowError>,
    /// More rows follow, or the head was cut.
    pub truncated: bool,
}

/// The outcome of an import.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ImportResult {
    /// The version the import made.
    pub version: i64,
    /// Items imported.
    pub imported: i64,
    /// Rows rejected, all of them.
    pub error_count: i64,
    /// The first 100 rejected rows.
    pub errors: Vec<ImportRowError>,
}
```

`ImportResult.version` is required: an import with no valid row answers 400 and makes no version (D-J-6), so a `201` always has one.

`rows` holds any JSON (arrays of strings for CSV, any value for JSONL), so its schema is an array of M1's any-JSON schema, never `Object`. Add in `api/datasets.rs`, next to the types, a private function with the return type M1 gave `crate::api::common::any_json` wrapped in an array:

```rust
/// OpenAPI schema of `ImportPreview.rows`: an array of any JSON values.
fn any_json_array() -> utoipa::openapi::schema::Array {
    utoipa::openapi::schema::ArrayBuilder::new().items(crate::api::common::any_json()).build()
}
```

If `any_json`'s return type does not convert into the `items` argument, wrap it as `ArrayBuilder::items` requires (`RefOr::T(Schema::Object(…))`); if `any_json` does not exist, STOP (M1 adds it).

### 2. Route table

In `routes()` of `routes/datasets.rs`, after M1's item routes:

```rust
RouteDef {
    method: Method::POST,
    path: "/api/v1/projects/{project}/datasets/{dataset}/import/preview",
    access: Access::Project { perm: ProjectPerm::View, session_only: false },
    documented: true,
    handler: post(preview_import).layer(DefaultBodyLimit::max(PREVIEW_BODY_LIMIT)),
},
RouteDef {
    method: Method::POST,
    path: "/api/v1/projects/{project}/datasets/{dataset}/import",
    access: Access::Project { perm: ProjectPerm::ManageDatasets, session_only: false },
    documented: true,
    handler: post(import_items).layer(DefaultBodyLimit::max(IMPORT_BODY_LIMIT)),
},
```

Constants in `routes/datasets.rs`:

```rust
/// The preview body: the head of the file (D-J-6).
const PREVIEW_BODY_LIMIT: usize = import::MAX_PREVIEW_BYTES;
/// The import body: the file plus the multipart framing and the mapping part.
const IMPORT_BODY_LIMIT: usize = import::MAX_FILE_BYTES + 1024 * 1024;
/// Imports parsed at once in this process; each holds its file in memory.
#[cfg(not(test))]
const IMPORTS_AT_ONCE: usize = 2;
/// Tests run many imports in parallel threads; they must not see 503.
#[cfg(test)]
const IMPORTS_AT_ONCE: usize = 64;
static IMPORTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(IMPORTS_AT_ONCE);
```

A `read` key may call the preview (it writes nothing); the access-suite expectations follow from `ProjectPerm::View`, so they need no hand edit. If the access suite's generic body (`send(…, body = true)`) makes the preview answer something it does not expect, read its failure and add the same kind of exception the suite already has for other raw-body routes; if there is none, STOP and report.

### 3. `preview_import`

```rust
async fn preview_import(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    Path(DatasetPath { dataset }): Path<DatasetPath>,   // M1's path extractor for {dataset}
    error::Query(params): error::Query<PreviewParams>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<ImportPreview>, ApiError>;
```

1. Resolve the dataset (M1) → 404 when absent. Reading it proves the caller may see it; the preview does not use its schemas.
2. `body`: a rejection whose `status()` is `413` → `ApiError::PayloadTooLarge { detail: "a preview takes at most 1 MiB" }`; any other rejection → `ApiError::BadRequest { detail: Some("the body cannot be read".into()), errors: vec![] }`.
3. `import::preview(&body, params.format, params.partial)` on `tokio::task::spawn_blocking` (a join error is `ApiError::Internal`). `FileError::Header(message)` → `BadRequest { detail: Some(message), errors: vec![] }`. (`Mapping` and `TooManyRows` cannot happen in a preview.)
4. Answer `200` with `ImportPreview { format, columns, rows, errors, truncated }`.

No audit line (nothing is written). One `tracing::debug!` with `dataset_id`, `project_id`, `bytes`, `format` is allowed.

### 4. `import_items`

```rust
async fn import_items(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    Path(DatasetPath { dataset }): Path<DatasetPath>,
    error::Query(params): error::Query<ImportParams>,
    multipart: Result<Multipart, MultipartRejection>,
) -> Result<(StatusCode, Json<ImportResult>), ApiError>;
```

1. `multipart` rejection (not `multipart/form-data`, no boundary) → `ApiError::UnsupportedMediaType { detail: "the import is multipart/form-data with a file part" }`.
2. Resolve the dataset → 404.
3. `let _permit = import_permit(&IMPORTS)?;` — `try_acquire`, and on failure `ApiError::Unavailable { detail: "another import is running; retry in a few seconds" }` (503). Hold the permit until the handler returns. The `Multipart` extractor has not read the body yet, so the permit is taken before any of the file is read (it bounds memory: at most 2 files in memory per process).
4. Read the parts in one function `async fn read_parts(multipart: Multipart) -> Result<Parts, ApiError>` (`struct Parts { file_name: String, file: Vec<u8>, mapping: Option<ImportMapping> }`, with a hand-written `Debug` printing only `file.len()` and whether a mapping is present), called as `upload_deadline(read_parts(multipart), UPLOAD_TIMEOUT).await??` where

```rust
/// The whole multipart body must arrive within this time, so a slow sender cannot hold an
/// import permit (D-J-6).
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(60);

/// `fut`, or 400 `the upload took too long` after `limit`.
async fn upload_deadline<T>(fut: impl Future<Output = T>, limit: Duration) -> Result<T, ApiError> {
    tokio::time::timeout(limit, fut).await.map_err(|_| ApiError::BadRequest {
        detail: Some("the upload took too long".into()),
        errors: vec![],
    })
}
```

   `read_parts` reads the parts in a loop with `multipart.next_field().await`:
   - a `MultipartError` whose `status()` is `413` → `PayloadTooLarge { detail: "the file is larger than 100 MiB" }`; any other → `BadRequest { detail: Some("the multipart body cannot be read".into()), … }`;
   - part `file`: keep `clean_file_name(field.file_name())`, then read its chunks with `field.chunk().await` into a `Vec<u8>`; when the total passes `import::MAX_FILE_BYTES`, answer `PayloadTooLarge { detail: "the file is larger than 100 MiB" }` at once;
   - part `mapping`: read it whole (at most 1 MiB, else `BadRequest` field `mapping`, message `the mapping is larger than 1 MiB`), then `serde_json::from_slice::<ImportMapping>` (failure → `BadRequest` with one `FieldError { field: "mapping", message: "invalid mapping JSON" }`);
   - any other part name → `BadRequest` field `<name>`, message `unknown part`;
   - a second `file` or `mapping` part → `BadRequest` field `<name>`, message `part repeated`.
   ```rust
   /// The file name as stored in the version description: control characters removed
   /// (`char::is_control`), then cut to 200 characters; `"upload"` when absent or empty after that.
   fn clean_file_name(name: Option<&str>) -> String;
   ```
5. No `file` part → `BadRequest` field `file`, message `required`. CSV without `mapping` → `BadRequest` field `mapping`, message `required for CSV`. JSONL with a `mapping` part → `BadRequest` field `mapping`, message `JSONL imports take no mapping`.
6. Keep the `DatasetRow` from step 2 as `used`. On `spawn_blocking`: `let schemas = Schemas::of(&used)?;` (once), parse (`parse_csv` or `parse_jsonl`), then `check_item("", &item, &schemas)` on every parsed item; a non-empty result moves that item to `errors` with its row and the message of the first `FieldError` as `format!("{}: {}", error.field, error.message)` (§ M1 names). Keep `errors` sorted by row. File errors map: `Mapping(errors)` → `BadRequest { detail: Some("invalid mapping".into()), errors }`; `Header(message)` → `BadRequest { detail: Some(message), errors: vec![] }`; `TooManyRows` → `BadRequest { detail: Some("the file has more than 50000 rows".into()), errors: vec![] }`.
7. No valid item → `BadRequest { detail: Some("no row could be imported".into()), errors }` where `errors` are the first `MAX_LISTED_ERRORS` row errors as `FieldError { field: format!("rows[{row}]"), message, position: None }`. Nothing is written.
8. Build `VersionChange { close: vec![], insert, description: format!("import {file_name} ({n} items)"), created_by: created_by(&principal) }`, where `insert` has one `NewRevision { revision_id: Uuid::now_v7(), item_id: Uuid::now_v7(), input, expected_output, metadata, source_trace_id: None, source_span_id: None }` per valid item, in file order. Write it with `write_checked(&state, access.project.id, &used, change, None).await?` (below). The live-item cap is M1's (409 `a dataset holds at most 50000 items`, inside `write_version`), checked against the valid rows.

   ```rust
   /// Writes `change` as one version of the dataset `used`, after re-reading the dataset in the
   /// write transaction (`get_dataset_tx` by id): gone → 404; `input_schema` or `expected_schema`
   /// text different from `used`'s → 409 `the dataset's schema changed; retry` and nothing written
   /// (D-J-2: the rows were validated against `used`). With `guard = Some((trace, users))`
   /// (add-from-trace, J2.4) the closure first asks `state.deletions.guard.blocking(project,
   /// trace, &borrowed users, now)` and answers 404 when it blocks (D-J-9). Returns the new
   /// version. The import passes `None`.
   async fn write_checked(state: &AppState, project: ProjectId, used: &DatasetRow,
       change: VersionChange, guard: Option<(TraceId, Vec<(String, i64)>)>) -> Result<i64, ApiError>;
   ```

   Inside the closure return a small private enum (`Written(i64)`, `Gone`, `SchemaChanged`, `Blocked`) and map it after the write, as M1's handlers map their closure results.
9. Audit: `tracing::info!(target: "audit", dataset_id = %id, project_id = %project, version, imported = n, rejected = error_count, format = ?params.format, <user_id or key_id as M1's handlers log it>, "dataset items imported");`.
10. Answer `201` with `ImportResult { version, imported: n, error_count, errors: first 100 }`.

The file name goes into the version description (stored, shown to project members); it never goes into a log.

### 5. OpenAPI

Register `ImportFormat`, `ImportColumn`, `ImportMapping`, `ImportRowError`, `ImportPreview`, `ImportResult` in `collect_schemas()` (and the imports at the top). In M1's `dataset_paths` add:

- `post_dataset_import_preview`, tag M1's datasets tag, summary `Preview the head of an import file`; parameters: `with_query::<PreviewParams>(vec![project_param(), <M1's dataset param>])`; request body: required, content `application/octet-stream` with `binary_string()`, description `The first bytes of the file, at most 1 MiB.`; responses `responses("200", json_of::<ImportPreview>("The columns and the first rows."), &[400, 401, 403, 404, 413, 429])`.
- `post_dataset_import`, summary `Import items from a CSV or JSONL file`; parameters `with_query::<ImportParams>(…)`; request body: required, content `multipart/form-data` with an object schema of two properties: `file` (`binary_string()`) and `mapping` (`Ref` to `ImportMapping`), `required: ["file"]`, description `A file part (at most 100 MiB) and, for CSV, a mapping part.`; build it with `RequestBodyBuilder` like `json_body` does, using `ObjectBuilder::new().property(…).required(…)`; responses `responses("201", json_of::<ImportResult>("The version the import made."), &[400, 401, 403, 404, 409, 413, 415, 429, 503])`.

Add a `503` description to `problem()` only if it has none (`post_evaluator_test` already declares 503: reuse it).

`crates/server/tests/openapi.rs`: add both operations to `OPERATIONS` (sorted as the list is), and to the `expected_errors` chain:

```rust
} else if *id == "post_dataset_import_preview" {
    expected_errors.extend(["400", "401", "403", "404", "413", "429"]);
} else if *id == "post_dataset_import" {
    expected_errors.extend(["400", "401", "403", "404", "409", "413", "415", "429"]);
```

If `document_lint` or another test in that file refuses a non-JSON request body, read the failing assertion: when it is a list of operation ids with an exception (as for `run_sql`), add these two ids there; otherwise STOP and report.

Run `just openapi`.

### 6. Fixtures

- `fixtures/api/requests/dataset-import-mapping.json`: `{"columns": [{"column": "question", "target": "input.question"}, {"column": "answer", "target": "expected"}, {"column": "topic", "target": "metadata.topic"}]}`. Add `roundtrip::<ImportMapping>("requests/dataset-import-mapping.json");` to `request_examples_match_their_types_and_the_openapi_document` (not to the `requests` array validated against `application/json` bodies: this one is a multipart part).
- `fixtures/api/static/dataset-import-preview.json`: an `ImportPreview` of `format: "csv"`, `columns ["question","answer","topic"]`, two rows of three strings, `errors []`, `truncated false`.
- `fixtures/api/static/dataset-import-result.json`: `{"version": 3, "imported": 2, "error_count": 1, "errors": [{"row": 2, "message": "the row has no input"}]}` (use the next version after M1's example dataset's `current_version`; read it from M1's fixtures).
- In `api_examples.rs`, register both static files as M1 registers its POST statics: `example(…)` with status 200 for the preview (`post_dataset_import_preview`, path param `dataset` = M1's example dataset id, query `format=csv`), `example_created("post_dataset_import", …)` for the result. In `tests/api_examples.rs`, add both to the static `roundtrip` list and to the `generated_files_roundtrip` match (`"post_dataset_import_preview" => roundtrip::<ImportPreview>(&example.file)`, `"post_dataset_import" => roundtrip::<ImportResult>(&example.file)`).
- Run `just api-examples`.

### Tests (`routes/datasets.rs` `mod tests`)

Helpers (in the test module):

```rust
/// A multipart body with a `file` part and an optional `mapping` part.
fn multipart(file_name: &str, file: &[u8], mapping: Option<&str>) -> (String, Vec<u8>);  // (content type, body)
```

Build it by hand: boundary `cetra-test-boundary`; each part `--{b}\r\nContent-Disposition: form-data; name="file"; filename="{file_name}"\r\nContent-Type: application/octet-stream\r\n\r\n{bytes}\r\n`; the mapping part with `name="mapping"` and `Content-Type: application/json`; the end `--{b}--\r\n`. Content type `multipart/form-data; boundary=cetra-test-boundary`.

Create the dataset with M1's route (`POST …/datasets`, `{"name": "faq"}`) or M1's test helper. Paths: `format!("/api/v1/projects/{}/datasets/faq/import?format=csv", seeded.project.id)`.

| Test | Steps | Assert |
|---|---|---|
| `preview_shows_the_head` | POST preview, body `q,a\nhi,x\nho,y\n`, `format=csv` | 200, `columns ["q","a"]`, 2 rows, `truncated` false |
| `preview_partial` | `partial=true`, body `q,a\nhi,x\nho` | 1 row, `truncated` true |
| `preview_bad_header` | body `q,q\n` | 400, detail `column 2 repeats the name of column 1` |
| `preview_over_one_mib` | body of 1 MiB + 1 byte | 413, code `payload_too_large` |
| `preview_with_a_read_key` | `Authorization: Bearer <seeded.read_key>` | 200 |
| `import_csv_makes_one_version` | CSV with 3 rows, row 2 has an empty input cell, mapping `q→input`, `a→expected` | 201, `imported 2`, `error_count 1`, `errors [{row 2, "the row has no input"}]`, `version` = old `current_version` + 1; `GET …/items` lists the 2 items; `GET …/versions` newest description `import data.csv (2 items)` |
| `import_jsonl` | `format=jsonl`, 2 valid lines and one `{"input":1,"x":2}` | 201, `imported 2`, `errors [{row 3, "unknown key x"}]` |
| `import_checks_the_schema` | dataset with `input_schema {"type":"object","required":["q"]}`; JSONL lines `{"input":{"q":1}}` and `{"input":{}}` | 201, `imported 1`, `errors [{row 2, "input: /: required"}]` (M1's `"<instance path>: <keyword>"` with the root path written `/`) |
| `a_schema_change_during_an_import_is_a_conflict` | create `faq`; `let used = find(&app.state, project, "faq").await?`; PATCH `faq` with `input_schema {"type":"object"}`; call `write_checked(&app.state, project, &used, change, None)` with one `NewRevision` | `Err(ApiError::Conflict { detail })` with detail `the dataset's schema changed; retry`; `GET …/versions` shows no new version |
| `the_file_name_is_cleaned` | `multipart("a\u{7}b\nc.csv", …)`, one valid row | the newest version's description is `import abc.csv (1 items)` |
| `clean_file_name_rules` (`#[test]`) | `None`, `Some("")`, `Some("\u{1}")`, `Some("a".repeat(250))` | `"upload"`, `"upload"`, `"upload"`, 200 `a`s |
| `upload_deadline_stops_a_slow_body` (`#[tokio::test]`) | `upload_deadline(std::future::pending::<()>(), Duration::from_millis(10))` | `Err(ApiError::BadRequest { detail: Some("the upload took too long"), .. })`; with `std::future::ready(1)`: `Ok(1)` |
| `import_without_valid_rows` | JSONL with only `{"expected_output":1}` | 400, detail `no row could be imported`, `errors[0].field == "rows[1]"`; `current_version` unchanged |
| `import_lists_at_most_100_errors` | JSONL with 1 valid line and 150 lines `[1]` | 201, `error_count 150`, `errors.len() == 100`, `errors[0].row == 2` |
| `import_too_many_rows` | JSONL of 50,001 lines | 400, detail `the file has more than 50000 rows` |
| `import_respects_the_live_cap` | seed the dataset with 49,999 items through the store (`write_version` in chunks of 1,000 `NewRevision`s), then import 2 rows | 409, detail `a dataset holds at most 50000 items`; no new version |
| `import_over_100_mib` | a `file` part of `MAX_FILE_BYTES + 1` bytes (`vec![b'a'; …]`) | 413, detail `the file is larger than 100 MiB` |
| `import_mapping_errors` | CSV with mapping `{"columns":[{"column":"nope","target":"input"}]}` | 400, `errors[0].field == "mapping.columns[0].column"` |
| `import_csv_needs_a_mapping` | CSV without the mapping part | 400, `errors[0].field == "mapping"`, message `required for CSV` |
| `import_jsonl_refuses_a_mapping` | JSONL with a mapping part | 400, message `JSONL imports take no mapping` |
| `import_not_multipart` | JSON body | 415 |
| `import_needs_manage_datasets` | `Authorization: Bearer <seeded.read_key>` | 403 |
| `import_permit_is_refused_when_busy` (`#[test]`, no app) | a local `Semaphore::new(1)`; take its permit; call `import_permit(&local)` | `Err(ApiError::Unavailable { .. })`; after dropping the permit, `Ok` |
| `import_logs_no_content` (`#[tokio::test]`, `capture_logs`) | CSV whose file name, header and cells contain `canary-import-2` | 201; the captured logs do not contain `canary-import-2` |

The `IMPORTS` semaphore is process-wide, so no HTTP test holds its permits (that would make the other import tests flaky). Step 3 goes through a small function the unit test calls with its own semaphore:

```rust
/// One import permit, or 503 when every one is taken.
fn import_permit(imports: &tokio::sync::Semaphore) -> Result<tokio::sync::SemaphorePermit<'_>, ApiError>;
```

The handler calls `import_permit(&IMPORTS)?`.

### Done when

- The tests pass; `just check` is green.
- `crates/server/openapi.json` has `post_dataset_import_preview` and `post_dataset_import`, the import body is `multipart/form-data`.
- `fixtures/api/manifest.json` lists both operations.

---

## J2.3 — Export

**Commit subject:** `server: export dataset versions as JSONL or CSV`

Adds `GET …/datasets/{dataset}/export?version=&format=` (D-J-7): the version's live items, streamed.

### Read first

- `crates/server/src/routes/sql.rs` `export_response` (headers, `Body::from_stream`, `Cache-Control: no-store`).
- `crates/server/src/meta/datasets.rs` (M1): `list_items` and the row type it returns; M1's stored-row → API item conversion.
- M1's version parameter check.

### Files

| File | Change |
|---|---|
| `crates/server/src/api/datasets.rs` | `ExportFormat`, `ExportParams` |
| `crates/server/src/routes/datasets.rs` | route and handler |
| `crates/server/src/api/openapi.rs`, `crates/server/tests/openapi.rs` | one operation |
| `crates/server/tests/api_examples.rs` | the manifest exception |
| `crates/server/openapi.json`, `web/src/api/schema.gen.ts` | regenerated |

### 1. Types

```rust
/// The format of an export (D-J-7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat { Jsonl, Csv }

/// `?version=&format=` of the export.
#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ExportParams {
    /// The version to export; default the latest.
    pub version: Option<i64>,
    /// Default `jsonl`.
    #[serde(default)]
    pub format: Option<ExportFormat>,
}
```

### 2. Route and handler

```rust
RouteDef {
    method: Method::GET,
    path: "/api/v1/projects/{project}/datasets/{dataset}/export",
    access: Access::Project { perm: ProjectPerm::View, session_only: false },
    documented: true,
    handler: get(export_items),
},
```

`export_items(State, Extension<ProjectAccess>, Path<DatasetPath>, error::Query<ExportParams>) -> Result<Response, ApiError>`:

1. Resolve the dataset (404), check `version` with M1's check (default `current_version`).
2. Build the body stream with `futures::stream::try_unfold` over `(cursor: Option<String>, first: bool, done: bool)`, using a clone of the reader pool (`state.meta.reader().clone()`) and the dataset id and version moved into the closure. Each step reads one page of `EXPORT_PAGE = 1_000` items with `list_items`, and returns one `Bytes` chunk:
   - JSONL: one line per item, `serde_json::to_vec` of an `ExportLine` (private, `Serialize`): `{id, input, expected_output?, metadata, source_trace_id?, source_span_id?}` (`skip_serializing_if = "Option::is_none"`), each followed by `\n`;
   - CSV: the first chunk starts with the header `id,input,expected_output,metadata,source_trace_id,source_span_id\r\n`; each item is written with a `csv::Writer` over a `Vec<u8>` (default settings: `\r\n` terminator, quotes as needed): `id`, compact JSON of `input`, compact JSON of `expected_output` or empty, compact JSON of `metadata`, the source ids in lowercase hex or empty.
   - the stream ends after a page shorter than `EXPORT_PAGE`. An empty version yields one chunk: the CSV header, or nothing for JSONL.
   - a `MetaError` ends the stream with `std::io::Error::other("dataset export failed")` after a `tracing::warn!` with `dataset_id`, `project_id`, `version` and the error's `Display` (a `MetaError` prints no content).
3. Headers: `Content-Type` `application/x-ndjson` or `text/csv; charset=utf-8`; `Content-Disposition: attachment; filename="<dataset id>-v<version>.<jsonl|csv>"`; `Cache-Control: no-store`. Answer `200` with `Body::from_stream(stream)`.

Reads in pages keep each SQLite read short; a version that changes during the export cannot change (versions are immutable), and the cursor is the item id, so no item is skipped or repeated.

### 3. OpenAPI and the manifest

`get_dataset_export`, summary `Export a dataset version`, parameters `with_query::<ExportParams>(vec![project_param(), <dataset param>])`, responses: a `200` built like `sql_result` with `application/x-ndjson` and `text/csv` content (`string_schema()`), description `The version's items, one per line (JSONL) or row (CSV), with Content-Disposition: attachment.`, and errors `&[400, 401, 403, 404, 429]`. Register `ExportFormat`.

`tests/openapi.rs`: add to `OPERATIONS`; in the `expected_errors` chain, add `get_dataset_export` with `["400", "401", "403", "404", "429"]` (or to `QUERY_OR_BODY` if that list is how the file groups query-parameter reads: follow the file).

`tests/api_examples.rs` `manifest_covers_the_read_operations`: add `&& id != "get_dataset_export"` to the exclusions, with the comment `// A file download, not a JSON body.` next to the stream exclusions.

Run `just openapi`.

### Tests (`routes/datasets.rs` `mod tests`)

| Test | Steps | Assert |
|---|---|---|
| `export_jsonl_of_a_past_version` | add 2 items (v1), patch one (v2), add 1 (v3); export `version=1` | 200, `content-type` `application/x-ndjson`, 2 lines, the first line's `input` is the original value; `content-disposition` ends with `-v1.jsonl"` |
| `export_csv` | 2 items, one with `expected_output` and `metadata {"t":"x"}`; `format=csv` | header line exact; 3 records when parsed with `csv::Reader`; the `input` cell parses as JSON equal to the item's |
| `export_pages` | 2,500 items (store helper), `format=jsonl` | 2,500 lines, ids strictly increasing |
| `export_empty_dataset` | version 0, `format=csv` | the header only |
| `export_bad_version` | `version=99` | 400 |
| `export_with_a_read_key` | `Bearer <read_key>` | 200 |
| `export_source_ids` | an item added with source ids through the store | its line has `source_trace_id` in lowercase hex |

Read the streamed body with `TestResponse`'s body text (as the SQL export tests do in `routes/sql.rs`).

### Done when

- The tests pass; `just check` is green; `get_dataset_export` is in `openapi.json`.

---

## J2.4 — Add from a trace or a span

**Commit subject:** `server: add dataset items from traces`

Adds `POST …/datasets/{dataset}/items/from-trace` (D-J-8, D-J-9) and a public helper in `cetra-eval` that names the trace root.

### Read first

- Design D-J-8 and D-J-9.
- `crates/server/src/eval/trace_load.rs` (`load_trace`, `LoadError`, `LoadedTrace`).
- `crates/server/src/eval/execute.rs`: `users_of`, `borrowed_users`, and how `run_job` calls `load_trace` and `state.deletions.guard.blocking` (step 6 and check A).
- `crates/server/src/deletion/guard.rs` (`blocking`, `add_traces`).
- `crates/eval/src/mapping/resolve.rs` (`TraceData::new`, `resolve`, the private `root_span`), `crates/eval/src/mapping/mod.rs` (`VariableMapping::from_json`, the `span.input` / `span.output` expressions and the `?` suffix).
- `crates/server/src/entitlements/window.rs` (`Visibility::window`, and the test `direct_storage_reads_are_listed`: this route reads through `load_trace`, which is in a listed file, so the list does not change).
- `fixtures/api/dataset/spans.json` (trace `a1000000000000000000000000000001`: root `b100000000000011`, input and output `text`; its span `b100000000000013` has `json` input and output; trace `…0005` is an evaluation trace).

### Files

| File | Change |
|---|---|
| `crates/eval/src/mapping/resolve.rs` | `TraceData::root_span_id` |
| `crates/server/src/api/datasets.rs` | `FromTraceRequest` |
| `crates/server/src/routes/datasets.rs` | route and handler |
| `crates/server/src/api/openapi.rs`, `crates/server/tests/openapi.rs` | one operation |
| `crates/server/openapi.json`, `web/src/api/schema.gen.ts` | regenerated |
| `fixtures/api/requests/dataset-from-trace.json`, `fixtures/api/static/dataset-item-from-trace.json` | new |
| `crates/server/src/api_examples.rs`, `crates/server/tests/api_examples.rs` | example, round-trips |
| `fixtures/api/manifest.json` | regenerated |
| `CHANGELOG.md` | `Added` lines for M2 |
| `docs/roadmap.md` | J's status |

### 1. `TraceData::root_span_id` (`cetra-eval`)

In `impl<'a> TraceData<'a>`:

```rust
/// The span `trace.*` expressions read: a span with no parent (or whose parent is not loaded),
/// the earliest by start time, then the lowest id; failing that, the earliest `llm` span.
/// `None` for an empty trace.
#[must_use]
pub fn root_span_id(&self) -> Option<SpanId> {
    root_span(self.spans).map(|span| span.span_id)
}
```

Test in `resolve.rs` `mod tests`: `root_span_id_is_the_trace_root` — with the module's existing span builders, a parentless span and a child: the parentless span's id; an empty slice: `None`.

Track J may add this public function to `cetra-eval`: it adds no dependency between crates (the server already depends on `cetra-eval`).

### 2. Type

```rust
/// `POST …/items/from-trace` (D-J-8).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FromTraceRequest {
    /// 32 hex digits, either case.
    pub trace_id: String,
    /// 16 hex digits. Absent: the trace root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,
    /// Replaces the span's input. Any JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(schema_with = crate::api::common::any_json)]
    pub input: Option<serde_json::Value>,
    /// Replaces the span's output. Any JSON; `null` stores no expected output.
    #[serde(default, …)]                      // the absent/null pattern of M1's UpdateItemRequest
    #[schema(schema_with = crate::api::common::any_json)]
    pub expected_output: Option<Option<serde_json::Value>>,
    /// Default `{}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
}
```

Use exactly the attribute M1 used for `UpdateItemRequest.expected_output` to tell "absent" from "null". `input: null` deserializes to `None` and means "use the span's input".

### 3. Route and handler

```rust
RouteDef {
    method: Method::POST,
    path: "/api/v1/projects/{project}/datasets/{dataset}/items/from-trace",
    access: Access::Project { perm: ProjectPerm::ManageDatasets, session_only: false },
    documented: true,
    handler: post(add_from_trace),
},
```

The default 1 MiB body limit applies (an edited input fits; the item limit is 1 MiB anyway).

Register it **before** M1's `…/items/{item_id}` routes in the list if M1's router would otherwise match `from-trace` as an `{item_id}` (axum matches a static segment before a parameter, so order does not matter for matching; check that a `GET …/items/from-trace` still answers 404 or 405 rather than 400 on a non-UUID item id, and leave it as it is).

`add_from_trace(State, Extension<Principal>, Extension<ProjectAccess>, Path<DatasetPath>, error::Json<FromTraceRequest>) -> Result<(StatusCode, Json<DatasetItem>), ApiError>`:

1. Parse `trace_id` with `TraceId::from_str` (400 field `trace_id`, message `must be 32 hex digits`) and `span_id` with `SpanId::from_str` (400 field `span_id`, `must be 16 hex digits`).
2. Resolve the dataset (404).
3. Load the trace: `eval::trace_load::load_trace(&state, project.id, Visibility::window(&state, project.org_id), trace_id, span_id, None)`. Map errors:
   - `NotFound`, `NoTargetSpan` → `ApiError::NotFound`;
   - `EvaluationTrace` → `BadRequest`, field `trace_id`, message `an evaluation trace cannot be added to a dataset`;
   - `TooLarge` → `BadRequest`, field `trace_id`, message `the trace is too large to load`;
   - `Busy`, `Saturated` → `ApiError::Unavailable { detail: "storage is busy; retry in a few seconds" }`;
   - `Failed`, `Convert` → `ApiError::Internal` (boxed error, as other routes do).
   Then `loaded.truncated` (the loader skipped spans past its caps, so the root it would pick may be wrong) → `BadRequest`, field `trace_id`, message `the trace is too large to copy` (D-J-8). Do steps 4–6 in a pure function `fn item_from(loaded: &LoadedTrace, span: Option<SpanId>, request: &FromTraceRequest) -> Result<(SpanId, ItemValues), ApiError>` so the test below can build a `LoadedTrace` by hand.
4. The source span: `span_id` when given, else `TraceData::new(&loaded.spans, None).root_span_id()` (`None` cannot happen after `load_trace` succeeded; map it to `NotFound`).
5. Resolve `{"input": "span.input?", "output": "span.output?"}` with `VariableMapping::from_json(&json!(…))` (the parse cannot fail; map an error to `Internal`) and `resolve(&mapping, Some(&TraceData::new(&loaded.spans, Some(source_span))), None)`. A `MappingError` → `Internal`. The decoded `input` and `output` are JSON values, `Null` when the span has none (spec 06 § Variable mapping: `messages` and `json` as JSON, `text` as a string).
6. The item: `input` = the request's `input`, else the decoded input; `Null` → `BadRequest`, field `input`, message `the span has no input`. `expected_output` = the request's when the key was sent (`Some(None)` → absent), else the decoded output, with `Null` → absent. `metadata` = the request's or `{}`. Then (in the handler, after `item_from`) `check_item("", &item, &Schemas::of(&used)?)` with `used` the `DatasetRow` of step 2: a non-empty result is `ApiError::BadRequest { detail: None, errors }`.
7. `let users = execute::users_of(&loaded.spans);` then write. What the one write transaction does, in order (J2.2's `write_checked` implements it):

```rust
// D-J-9: checked inside the write transaction, so a deletion that registers its guard entry
// before this check refuses the copy, and one that registers after it finds the item when its
// own write deletes the scope's dataset items.
if state.deletions.guard.blocking(project_id, trace_id, &execute::borrowed_users(&users), state.clock.now_ns()).is_some() {
    return Ok(Outcome::Blocked);
}
// Then the same re-read and schema comparison as `write_checked` (J2.2), and `write_version`
// with one `NewRevision` (both ids `Uuid::now_v7()`, `source_trace_id: Some(trace_id)`,
// `source_span_id: Some(source_span)`), description `add from trace <trace hex>`.
```

   In the handler this is one call: `write_checked(&state, project_id, &used, change, Some((trace_id, users))).await?` (J2.2's helper runs the guard check, then the schema re-check, then `write_version`, all in one transaction), with `users` the owned form of `execute::users_of(&loaded.spans)` that `borrowed_users` borrows from. `NewRevision` already carries the source ids (M1): M2 changes nothing in M1's store.
8. Audit: `tracing::info!(target: "audit", dataset_id, project_id, version, item_id, <user or key id>, "dataset item added from a trace")`. The trace id is not personal content, but keep the line to the ids M1's audit lines carry.
9. Answer `201` with the stored item (M1's conversion: `source_trace_id`, `source_span_id` in lowercase hex).

### 4. OpenAPI

`post_dataset_item_from_trace`, summary `Add an item copied from a trace or a span`, request `json_body("FromTraceRequest")`, responses `responses("201", json_of::<DatasetItem>("The new item."), &[400, 401, 403, 404, 409, 429, 503])`. Register `FromTraceRequest`. `tests/openapi.rs`: `OPERATIONS`, and

```rust
} else if *id == "post_dataset_item_from_trace" {
    expected_errors.extend(["400", "401", "403", "404", "409", "429"]);
```

(the lint collects 4xx codes only). Run `just openapi`.

### 5. Fixtures

- `fixtures/api/requests/dataset-from-trace.json`: `{"trace_id": "a1000000000000000000000000000001", "expected_output": "Your order 1234 shipped and arrives tomorrow."}`. Add it to the `requests` array of `request_examples_match_their_types_and_the_openapi_document` as `("post_dataset_item_from_trace", "requests/dataset-from-trace.json")`, and `roundtrip::<FromTraceRequest>(…)` to the same test.
- `fixtures/api/static/dataset-item-from-trace.json`: a `DatasetItem` for that request: the next item id of the design's `…0dxx` range not used by M1 (read M1's fixtures; e.g. `01900000-0000-7000-8000-000000000d04` with revision `…0d15`), `input "My order 1234 hasn't arrived."`, the expected output above, `metadata {}`, `source_trace_id "a1000000000000000000000000000001"`, `source_span_id "b100000000000011"`, `version` = the import example's version + 1. Register with `example_created("post_dataset_item_from_trace", …)`, round-trip as `DatasetItem` in both lists of `tests/api_examples.rs`. Add the two ids to the fixed-id table of `fixtures/api/README.md` next to M1's.
- `just api-examples`.

### 6. CHANGELOG and roadmap

`CHANGELOG.md` § `[Unreleased]` → `### Added`, one line:

```
- Datasets: import items from CSV (with a column mapping) or JSONL, export a version as JSONL or CSV, and add an item from a trace or a span (track J).
```

`docs/roadmap.md`, J's row: in the Status cell, add `M2 (import, export, add-from-trace) implemented.` after what M1 wrote there.

### Tests (`routes/datasets.rs` `mod tests`)

Every test loads the example data: `app.load_example_dataset(seeded.project.id).await` and creates the dataset `faq` (no schemas unless stated). `T1 = "a1000000000000000000000000000001"`.

| Test | Request | Assert |
|---|---|---|
| `from_trace_copies_the_root` | `{"trace_id": T1}` | 201; `input "My order 1234 hasn't arrived."`, `expected_output "Your order 1234 shipped and arrives tomorrow."`, `source_span_id "b100000000000011"`; `current_version` + 1; the version's description `add from trace a1000000000000000000000000000001` |
| `from_span_decodes_json` | `{"trace_id": T1, "span_id": "b100000000000013"}` | `input {"order_id":"1234"}`, `expected_output {"status":"shipped","eta":"2026-01-02"}` |
| `edited_values_replace_the_span` | `{"trace_id": T1, "input": {"q": "x"}, "expected_output": null, "metadata": {"k": 1}}` | `input {"q":"x"}`, no `expected_output`, `metadata {"k":1}` |
| `uppercase_ids_are_accepted` | `trace_id` in uppercase | 201 |
| `unknown_trace` | `trace_id` `a10000000000000000000000000000ff` | 404 |
| `unknown_span` | `span_id` `b1000000000000ff` | 404 |
| `evaluation_trace_is_refused` | trace `a1000000000000000000000000000005` | 400, field `trace_id` |
| `bad_ids` | `trace_id "xyz"` | 400, field `trace_id` |
| `schema_applies` | dataset with `input_schema {"type":"object"}`, root input is a string | 400, field `input` (M1's message) |
| `hidden_trace_is_not_found` | `set_limits` with `data_access_days: Some(1)` for the org (see `test_support::set_limits` and P's window tests), clock at 2026-01-03 so the example traces of 2026-01-01 are outside the window | 404 |
| `blocked_trace_is_refused` | `app.state.deletions.guard.add_traces(project, &[T1], Uuid::now_v7(), app.state.clock.now_ns())`, then the request | 404; no new version |
| `a_truncated_trace_is_refused` (`#[test]`) | `item_from` with a `LoadedTrace { truncated: true, spans: <the example trace's spans>, .. }` built by hand (read the struct's fields in `trace_load.rs`) | `Err(BadRequest)`, field `trace_id`, message `the trace is too large to copy` |
| `from_trace_rechecks_the_schema` | `used` = `find(…, "faq")`, then PATCH `faq` with an `input_schema`, then `write_checked(…, &used, change, Some((T1, vec![])))` | 409 `the dataset's schema changed; retry` |
| `read_key_is_refused` | `Bearer <read_key>` | 403 |
| `from_trace_logs_no_content` (`#[tokio::test]`, `capture_logs`) | `input` `"canary-trace-3"` | 201; logs do not contain `canary-trace-3` |

### Done when

- The tests pass; `just check` is green.
- `cargo test -p cetra-eval root_span_id` passes.
- `openapi.json` has `post_dataset_item_from_trace`; the manifest lists it.
- `CHANGELOG.md` and `docs/roadmap.md` are updated.

## Report

When M2 is done, report what was done, any deviation from this plan or the design and why, the exact versions of the new dependency (`csv 1.4.0`) and the axum feature added (`multipart`), and anything left undone or unverified.
