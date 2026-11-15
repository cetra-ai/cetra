# Strata v0 · M5: DataFusion provider and fast paths

Implementation plan for phases 6 and 7 of [the prototype design](../prototype-plan.md), § S8 and § S9. Revision 2, after the first cold review, which built the plan's dependencies and checked the code it names. There are two sub-phases. Each is done by one Haiku agent in one go, in order, and ends with the checks green and one commit.

**Prerequisites:** M4.3 is committed (`scan`, `ScanRequest`, `Predicate`, `Lit`, `lookup_scan`, `IoStats`).

**Read first:**
- [`01-m1-container-narrow.md`](01-m1-container-narrow.md): § Versions and § Conventions.
- [`03-m3-indexes-maps.md`](03-m3-indexes-maps.md): § Global rules.
- [`04-m4-scan.md`](04-m4-scan.md): § Rules for every sub-phase, and the semantics of `Predicate` (§ M4.2 and § M4.3 steps 4–5).
- [`05-datafusion-api.md`](05-datafusion-api.md): the DataFusion 55.2 API reference. **Use its signatures; do not use older DataFusion examples.**
- [`../prototype-plan.md`](../prototype-plan.md): § S8 and § S9.
- The code in `docs/research/file-format/strata/src/`: `scan.rs`, `predicate.rs`, `file.rs`, `lib.rs`.
- `docs/research/file-format/bench/src/main.rs`, functions `generate` and `query`, for the dataset and the queries.

| Sub-phase | Content |
|---|---|
| M5.1 | `StrataTable` and `StrataExec`: schema, filter translation and pushdown, partitions, ordering, metrics; equivalence tests against Parquet |
| M5.2 | fast paths `trace_detail` and `trace_list`, tested against DataFusion |

---

## M5.1: `StrataTable` and `StrataExec`

**Goal:** a DataFusion `TableProvider` over a set of Strata files that:
- pushes down every filter Strata evaluates exactly, and marks it `Exact`;
- pushes down substring `LIKE` as `Inexact`;
- uses `lookup_scan` for a `trace_id` lookup;
- returns the same rows as DataFusion's Parquet reader for every bench query.

**Edit:**
- `strata/Cargo.toml` and `strata/Cargo.lock`. The lock file is tracked and gains about 70 packages; commit it.
- `strata/src/{datafusion.rs (new),file.rs,lib.rs}`;
- `strata/tests/{datafusion.rs (new),common/dataset.rs (new)}`;
- `docs/research/file-format/prototype-plan.md` § S8.

**Code rules:**
- **Test modules** in `src` carry `#[cfg(test)] #[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]`.
- **No indexing in non-test code:** for example, `file.directory().get(c).is_some_and(|r| r.layer == Layer::Narrow)`, never `directory()[c]`.
- **Shared test file.** Integration tests reach the shared file with `#[path = "common/dataset.rs"] mod dataset;`, not `mod common;`: `tests/` has no `common/mod.rs`. `dataset.rs` starts with `#![allow(dead_code)]`, as `counting.rs` does.
- **Crate doc.** Update the `lib.rs` crate doc, which says "there is no DataFusion integration (M5)".

### Dependencies (`strata/Cargo.toml`)

- `[dependencies]`:
  - add `datafusion = { version = "55.2", default-features = false, features = ["sql", "nested_expressions"] }`;
  - move `async-trait = "0.1.92"` from `[dev-dependencies]` to here.
- `[dev-dependencies]`: add `datafusion = { version = "55.2", default-features = false, features = ["sql", "nested_expressions", "parquet", "string_expressions", "unicode_expressions", "datetime_expressions", "regex_expressions"] }`. These are the bench crate's features.
- Run `cargo +1.99.1 tree -e normal,dev --prefix none | grep -E '^(arrow|parquet|object_store) ' | sort -u`. It must print exactly one line per crate. A reviewer checked that this resolves to arrow 59.3.2, parquet 59.3.2 and object_store 0.13.2. If it prints more, stop and report.

### Types (`datafusion.rs`)

`lib.rs` adds `mod datafusion;` and `pub use datafusion::{StrataExec, StrataTable, table_schema};`. Inside `datafusion.rs`, refer to the external crate as `::datafusion`, since the module has the same name.

```rust
/// A DataFusion table over Strata files that share one schema.
#[derive(Debug)]
pub struct StrataTable {
    files: Vec<Arc<StrataFile>>,
    /// The table schema: the file schema with top-level Utf8 → Utf8View, Binary → BinaryView.
    schema: SchemaRef,
}

impl StrataTable {
    /// Opens every file in `paths` from `store`.
    pub async fn open(store: Arc<dyn ObjectStore>, paths: Vec<Path>, config: &Config) -> Result<StrataTable>;
    /// A table over already-open files.
    pub fn new(files: Vec<Arc<StrataFile>>) -> Result<StrataTable>;
}

/// The table schema of a file schema: top-level Utf8 → Utf8View and Binary → BinaryView, other
/// fields unchanged (names, nullability, metadata kept).
pub fn table_schema(file_schema: &Schema) -> SchemaRef;

/// Casts a batch in a file (sub)schema to the matching table (sub)schema.
pub(crate) fn to_table_batch(batch: &RecordBatch, table_schema: &SchemaRef) -> Result<RecordBatch>;
```

- **`new`.**
  - Every file's `schema()` must equal the first one's, compared with `==` on `Schema`. Otherwise `Schema { reason: "files have different schemas" }`.
  - Every file must have the same `sort_key_column()`, the existing `pub(crate)` accessor in `file.rs`. Use it everywhere this plan needs the sort key, and add no other accessor.
  - With no file, the error is `Schema { reason: "no file" }`.
  - `open` opens the files concurrently (`try_join_all` of `StrataFile::open(.., None, config)`), then calls `new`.
- **`Debug` for `StrataFile`** (`file.rs`). `TableProvider` and `ExecutionPlan` require `Debug`, and `StrataFile` has none. Its fields are not all `Debug`, so derive is impossible. Add a manual `impl std::fmt::Debug for StrataFile` that prints `path`, `size` and `row_count`, ending with `finish_non_exhaustive()`. This is the only `file.rs` change.
- **`to_table_batch`** casts each column with `arrow::compute::cast` to the table field's type. It builds the batch with `RecordBatch::try_new_with_options` and the batch's row count, so that zero-column batches work.
- **Errors.** `StrataError` maps to DataFusion with `DataFusionError::External(Box::new(e))`. Add `fn df(e: StrataError) -> DataFusionError` and use `.map_err(df)`.

### Filter translation (`datafusion.rs`)

Imports for the translation code:
- `datafusion::logical_expr::{Expr, Operator}`;
- `datafusion::logical_expr::expr::{BinaryExpr, Between, InList, Like, ScalarFunction}`;
- `datafusion::common::ScalarValue`.

`Expr::Column(c)` gives `c.name`.

```rust
/// How one filter expression is pushed down.
#[derive(Debug, PartialEq)]
enum Pushed {
    /// Strata evaluates it exactly.
    Exact(Predicate),
    /// Strata applies it as a superset; DataFusion re-checks.
    Inexact(Predicate),
    /// Not pushed.
    No,
}

fn translate(expr: &Expr, table: &StrataTable) -> Pushed;
```

`translate` is pure. `supports_filters_pushdown` and `scan` both call it, so they always agree.

**Leaves.** Shapes not in the table are `No`. A column is an `Expr::Column`, matched by `name` only through `schema.index_of`; a name that is not in the schema is `No`.

| Expr shape | Result |
|---|---|
| `BinaryExpr { Column c, op, Literal(v, _) }`, with `op` one of `Eq`, `NotEq`, `Lt`, `LtEq`, `Gt`, `GtEq` | `Exact(Cmp(c, op, lit))` |
| the same with the literal on the left | `Exact(Cmp(c, flipped op, lit))`; `Lt` ↔ `Gt` and `LtEq` ↔ `GtEq` flip, `Eq` and `NotEq` stay |
| `InList { expr: Column c, list: all literals, negated: false }` | `Exact(In(c, lits))` |
| `InList { negated: true, .. }` | `Exact(Not(In(c, lits)))` |
| `IsNull(Column c)`, `IsNotNull(Column c)` | `Exact(IsNull(c))`, `Exact(IsNotNull(c))` |
| `Between { expr: Column c, negated, low: Literal, high: Literal }` | `Exact(And([Cmp(c, GtEq, low), Cmp(c, LtEq, high)]))`, wrapped in `Not` when `negated` |
| `BinaryExpr { ScalarFunction get_field [Column m, Literal(Utf8(Some(k)))], Eq, Literal(Utf8(Some(v)) or Utf8(None)) }` | see § Map keys |
| `Like { negated: false, case_insensitive: false, escape_char: None, expr: Column c, pattern: Literal(Utf8View or Utf8 (Some(p))) }` | see § LIKE |

- **Column checks.** The column of a `Cmp`, `In`, `IsNull` or `IsNotNull` leaf must be a narrow column in every file (`directory()[c].layer == Layer::Narrow`, checked on every file), and its type must be one of M4.2's comparable types. Otherwise `No`. M4.2's `bind` rules decide which types are comparable; `IsNull` and `IsNotNull` accept any narrow type.
- **Literal conversion.** A `ScalarValue` becomes a `Lit` only when its type matches the column's **file** type, as below. Any other pair is `No`; that includes a `Cast` anywhere, which simply does not match the shapes above.

  | Column file type | `ScalarValue` | `Lit` |
  |---|---|---|
  | Boolean | `Boolean(v)` | `Bool` |
  | Int8–Int64 | the same `IntN(v)` | `Int(v as i64)` |
  | UInt8–UInt64 | the same `UIntN(v)` | `UInt(v as u64)` |
  | Float32, Float64 | `Float32(v)`, `Float64(v)` | `Float(v as f64)` |
  | Timestamp(Nanosecond, _) | `TimestampNanosecond(v, _)` (any timezone; the value is UTC nanoseconds) | `Int(v)` |
  | Utf8 | `Utf8View(v)` or `Utf8(v)` | `Utf8` |
  | Binary | `BinaryView(v)` or `Binary(v)` | `Bytes` |
  | FixedSizeBinary(w) | `FixedSizeBinary(w, v)` with the same `w` | `Bytes` |

  `v = None` gives `Lit::Null`.
- **Combinators.**
  - `BinaryExpr { a, And, b }` and `BinaryExpr { a, Or, b }` translate `a` and `b`. With either side `No`, the whole is `No`. Both sides `Exact` give `Exact(And/Or)`. Otherwise the result is `Inexact`.
  - `Not(e)`: `Exact(p)` gives `Exact(Not(p))`; anything else gives `No`.

**Map keys.**
- `get_field(m, k) = v` is `Exact(MapKeyEq(m, k, Utf8(v)))` only if, in **every** file:
  - `m` is a `MapParent`;
  - `k` is one of its promoted keys;
  - that key's `residual_hits == 0`.

  Check it per file with `crate::predicate::layout_of(file, m)`, which returns `Err` when `m` is not a shredded map; treat that as `No`. Then `layout.keys.iter().find(|key| key.key == k.as_bytes())` and its `residual_hits == 0`.

  Otherwise it is `No`.
- **Why.** DataFusion's `get_field` returns the value of the **first** entry with the key, while `MapKeyEq` returns NULL for a duplicated key. They agree only when the key never reaches the residual. A key with `residual_hits == 0` occurs at most once and never with a null value.

**LIKE.**
- `p` must have the form `%lit%`, where `lit` holds 1 to 64 bytes and none of `%`, `_` or `\`.
- `c` must be a payload column in every file.
- Then it is `Inexact(Contains(c, lit bytes))`; otherwise `No`.
- `Contains` counts a null row as FALSE instead of NULL. That is why it is never wrapped in `Not` (`Not` of an `Inexact` is `No`, as above), and why it is `Inexact`.

### `StrataTable` as `TableProvider`

- **`schema`** returns the table schema; **`table_type`** returns `Base`.
- **`supports_filters_pushdown`:** one entry per filter: `Exact`, `Inexact` or `Unsupported`, from `translate`.
- **`scan(state, projection, filters, limit)`:**
  1. **Predicate.** Translate every filter. Keep the predicates of `Exact` and `Inexact` results and drop `No`, which DataFusion keeps above the scan. `And` the kept predicates, or use `None` when there are none.
  2. **Projection.** `projection.cloned().unwrap_or_else(|| (0..n_fields).collect())`. The output schema is `project_schema(&self.schema, projection)`.
  3. **Lookup key.** If some filter is exactly `BinaryExpr { Column c, Eq, Literal(FixedSizeBinary(w, Some(bytes))) }`, call `bytes` the `key`, provided that:
     - `c` resolves through the schema to the index the files' `sort_key_column()` returns;
     - the column is `FixedSizeBinary(w)` with the same `w`, between 1 and 16.

     The column name is not hard-coded. A `None` literal, or any other width, means no lookup.
     - For each file, `file.may_contain(name, &key).await`; drop the files whose answer is `Some(false)`.
     - If that filter is the **only** kept filter and `limit` is `None`, the exec is in **lookup mode**: it calls `lookup_scan(&key, &projection)` per file.
  4. **Exec.** Build `StrataExec` with the remaining files, the projection, the predicate, the lookup key (lookup mode only), `limit`, and the output schema.

### `StrataExec`

```rust
/// Reads Strata files for one table scan.
#[derive(Debug)]
pub struct StrataExec {
    files: Vec<Arc<StrataFile>>,
    projection: Vec<usize>,
    predicate: Option<Predicate>,
    lookup: Option<Vec<u8>>,
    limit: Option<usize>,
    schema: SchemaRef,            // output (table) schema
    props: Arc<PlanProperties>,
    metrics: ExecutionPlanMetricsSet,
}
```

**Partitions.**
- In lookup mode with at most one file, and in every case with no file: **1** partition.
- Otherwise one partition per file.
- Partition `i` reads file `i`. In lookup mode with 0 or 1 files, partition 0 reads that file, or nothing.

**Ordering.**
- Declare one ordering, `[Column(name of the sort-key column, its index in the output schema)]` with `SortOptions { descending: false, nulls_first: false }`, only if:
  - every partition reads at most one file;
  - the sort-key column is in the projection.
- Otherwise declare none.
- `PlanProperties::new(eq, Partitioning::UnknownPartitioning(n), EmissionType::Incremental, Boundedness::Bounded).with_scheduling_type(SchedulingType::Cooperative)`.
  - Declaring `Cooperative` avoids a `CooperativeExec` wrapper.
  - This is a prototype shortcut: the single-future stream does not consume DataFusion's cooperative budget. Say so in a comment.

**`execute(partition, _)`:**
- **Partition check.** `partition >= n` gives `DataFusionError::Internal`.
- **Metrics.** Create `output_rows(partition)`, `elapsed_compute(partition)` and the counters `strata_bytes_fetched`, `strata_requests` and `strata_waves` (`counter(name, partition)`).
- **The future.** It moves clones of the file `Arc` and the fields in, and runs:
  1. Read `io_stats()` before.
  2. Call `lookup_scan(key, &projection)` in lookup mode. Otherwise call `scan(&ScanRequest { projection, predicate, row_ranges: None, limit, kind: ScanKind::Scan })`.
  3. Map each batch with `to_table_batch(&b, &schema)`.
  4. Read `io_stats()` after, and add the differences to the three counters. If several queries share a file concurrently, the counts mix; this is accepted for the prototype.
  5. Add the row total to `output_rows`.

  Time the CPU part with `elapsed_compute.timer()`, around steps 3 and 5 only.
- **Stream.** Wrap the future into a stream as in the API reference § 3.

**Other methods:**
- `name` is `"StrataExec"`.
- `fmt_as` writes `StrataExec: files=<n>, lookup=<bool>, predicate=<Debug of the predicate or none>`.
- `metrics` returns `Some(self.metrics.clone_inner())`.
- The leaf methods follow the API reference § 2.

### Design doc (§ S8)

Update the pushdown table and the metrics list to match this plan:
- `MapKeyEq` is `Exact` only when the key is promoted with `residual_hits == 0` in every file, and `Unsupported` otherwise. Explain why: `get_field` returns the first match.
- `strata_chunks_pruned` and `strata_frames_read` are dropped. The prototype's `scan` does not report them; `IoStats` covers I/O.
- Lookup mode is used only when the `trace_id` filter is the only filter.

### Tests

**Unit tests** (`datafusion.rs`, `#[tokio::test]`).
- **Main file.** `testgen::wide_batch(3_000)` written with `StrataWriter::new(wide_schema(), &["trace_id", "start_time"], Config::default())`, put in an `InMemory` store and opened with `StrataFile::open`. The open helpers of other test modules are private, so write a local one.
- **Column indexes.** `Predicate`s use schema indexes: look them up with `schema.index_of`. In `wide_schema()`, `input_tokens` is 7, `status` 11 and `attrs` 6.
- **Second file** for the positive `Like` and `MapKeyEq` cases. With the default config, no `wide_batch` column is payload. Write a small file with `Config { payload_min_avg: 1, .. }` and a schema of:
  - `trace_id FixedSizeBinary(16)`;
  - `start_time Timestamp(ns, "UTC")`;
  - `body Utf8`;
  - `m Map<Utf8, Utf8>`, where every row has the key `"k"` once with a non-null value.

- **`translate`.** Build each `Expr` with `datafusion::logical_expr::{col, lit, cast, Expr}`, and compare the result with the expected `Pushed`:
  - `col("status").eq(lit(ScalarValue::Utf8View(Some("ok".into()))))` gives `Exact(Cmp(11, Eq, Utf8("ok")))`.
  - `lit(5i64).lt(col("input_tokens"))` gives `Exact(Cmp(7, Gt, Int(5)))`.
  - On the second file:
    - `col("body").like(lit(ScalarValue::Utf8View(Some("%abc%".into()))))` gives `Inexact(Contains(..))`.
    - `get_field(col("m"), "k") = '"v"'` gives `Exact(MapKeyEq(..))`. Build the `get_field` call by planning the SQL `m['k'] = 'v'` against the table, or with `datafusion::functions::core::expr_fn::get_field`.
  - `attrs['k0']` on the main file gives `No`, because null values put `k0` in the residual.
  - `cast(col("input_tokens"), DataType::Float64).gt(lit(1.0))` gives `No`.
  - `col("input_tokens").in_list(vec![lit(1i64), lit(2i64)], true)` gives `Exact(Not(In(..)))`.
  - A `Cast` on the column gives `No`.
  - `lit(ScalarValue::Utf8(..))` on `input_tokens` gives `No`.
  - `col("tags").eq(..)`, a List column, gives `No`.
  - `col("uid").like(lit("%abc%"))` on a narrow column gives `No`.
  - An `Or` of `Exact` and `No` gives `No`.
  - `Not` of a `Like` gives `No`.
- **`table_schema`:** Utf8 becomes Utf8View, Binary becomes BinaryView, and the `Map` and `List` fields are unchanged.

**Equivalence tests** (`tests/datafusion.rs`, a single `#[ignore]` test `datafusion_equivalence` that runs in release, with shared setup in `tests/common/dataset.rs`).

`dataset.rs` provides `pub fn spans_dataset(traces: usize, files: usize) -> Dataset`, with `pub struct Dataset { pub files: Vec<Vec<RecordBatch>>, pub trace_ids: Vec<String>, pub user_id: String, pub customer_id: String }`:
- `trace_ids` holds 100 hex trace ids, written by a local `hex` function, since bench's is private;
- `user_id` is the `user_id` of the first span that has one;
- `customer_id` is the `metadata` value of `customer_id` of the first span, JSON-quoted as stored.

It copies the logic of bench `generate`:
- `World::new(7)` and `Rng::new(42)`;
- the `zqxjneedle` planting loop;
- the trace-id sample;
- the spans sorted by `start_time`, cut into `files` equal time slices, and each slice sorted by `(trace_id, start_time)`;
- `spans_to_record_batch` in chunks of 8,192 rows.

Use `traces = 2_000` and `files = 3`.

The test:
1. Writes each slice as a Strata file (`StrataWriter`, sort key `["trace_id", "start_time"]`, default config) into an `InMemory` store, and as a Parquet file (`parquet::arrow::ArrowWriter` with default properties) into a fresh directory `std::env::temp_dir().join(format!("strata-df-{}", std::process::id()))`. The directory is removed at the end.
2. Builds two `SessionContext`s:
   - `st`, with `register_table("t", Arc::new(StrataTable::open(..)))`;
   - `pq`, with `register_parquet("t", dir_str, ParquetReadOptions::default())`, which reads Utf8 as Utf8View by default. `dir_str` comes from `dir.to_str()`; a `None` is a test failure.
3. Runs every bench query on both. Copy the query list from bench `query`, with `t0 = datagen::DAY_START`, `h = 3_600 * datagen::NS`, and these changes:
   - `list_llm_1h` uses `ORDER BY start_time DESC, trace_id DESC, span_id DESC LIMIT 50` (design S9);
   - `TRACE` and `TRACE_LIGHT` become 5 queries each, over the first 5 ids;
   - `user_filter` uses `dataset.user_id`, and `metadata_filter` uses `dataset.customer_id`. With 2,000 traces, the bench's fixed values would match nothing;
   - two ordering queries are added: `SELECT trace_id FROM t ORDER BY trace_id`, and the same with `LIMIT 20`.

   **Non-empty results.** Assert on the Strata side that:
   - `count_errors_6h > 0`;
   - `list_llm_1h` has 50 rows;
   - `substring_input`, `user_filter` and `metadata_filter` each count more than 0.
4. Compares the results with `fn rows(batches: &[RecordBatch]) -> Vec<String>`:
   - for each row, join the column values formatted with `arrow::util::display::ArrayFormatter::try_new(col, &FormatOptions::default().with_null("<null>"))`;
   - cast Utf8View, BinaryView and LargeUtf8 columns to Utf8 or Binary first;
   - top-level Float32 and Float64 columns, aggregates included, take a separate branch: each value is `format!("{:.9e}", v)` and a null is `"<null>"`, so that sums in a different order compare equal.

   Queries without `ORDER BY` compare the sorted row lists; the others compare them in order.
5. **Pushdown checks.** For these queries, check whether `FilterExec` appears in the Strata physical plan: `ctx.sql(q).await?.create_physical_plan().await?`, displayed with `datafusion::physical_plan::displayable(plan.as_ref()).indent(true).to_string()`.
   - `count_errors_6h`, `user_filter` and `metadata_filter`: no `FilterExec`, because every filter is `Exact`. For `metadata_filter`, first assert that `customer_id` is a promoted key with `residual_hits == 0` in every file. Read it through a `pub(crate)` helper; since the test is in `tests/`, expose `StrataTable::exact_map_key(&self, column: &str, key: &str) -> bool`, a public method documented as a diagnostic. If the assertion fails, stop and report: the bench query would not be exact.
   - `substring_input`: a `FilterExec` remains, because the filter is `Inexact`, and the `StrataExec` line contains `Contains`.
   - `SELECT trace_id FROM t ORDER BY trace_id`: the plan has no `SortExec`, because each partition is one sorted file. A `SortPreservingMergeExec` is expected. Its rows equal Parquet's in order.
6. **Lookup mode.** For the first trace id, the plan string contains `lookup=true` and `files=1` (the other files are dropped by the key filter or zones). If a file is not dropped because of a filter false positive, accept `files<=3` and say so in the report.

### Done when

```bash
cargo +1.99.1 test
cargo +1.99.1 clippy --all-targets -- -D warnings
cargo +1.99.1 fmt --check
cargo +1.99.1 test --release -- --ignored roundtrip_payload roundtrip_light scan_datagen datafusion_equivalence --nocapture
```

**Commit:** `strata: add a DataFusion table provider with exact pushdown (M5.1)`

---

## M5.2: fast paths

**Goal:** `trace_detail` and `trace_list` answer the two most common UI queries without DataFusion, and return the same rows as the DataFusion queries.

**Edit:**
- `strata/src/{fast.rs (new),lib.rs}`;
- `strata/tests/{fast_paths.rs (new)}`;
- `docs/research/file-format/prototype-plan.md` § S9.

### API (`fast.rs`)

`lib.rs` adds `mod fast;` and `pub use fast::{trace_detail, trace_list};`.

```rust
/// Every span of one trace across `files`, with the named columns, in the table schema.
pub async fn trace_detail(files: &[Arc<StrataFile>], trace_id: [u8; 16], projection: &[&str]) -> Result<RecordBatch>;

/// The `k` latest spans with `start_time` in `time` and matching `predicate`, ordered by
/// (start_time DESC, trace_id DESC, span_id DESC), with the named columns, in the table schema.
pub async fn trace_list(files: &[Arc<StrataFile>], time: Range<i64>, predicate: Option<Predicate>,
                        k: usize, projection: &[&str]) -> Result<RecordBatch>;
```

**Common rules.**
- `files` must not be empty, and share one schema (as `StrataTable::new`); otherwise `Schema`.
- Names resolve with `schema.index_of(name)`; an unknown name is `Schema { reason: "no column <name>" }`.
- The output schema is `table_schema(file schema)` projected on the names, in order. Batches are converted with `to_table_batch`.
- An empty result is `RecordBatch::new_empty(output schema)`.

### `trace_detail`

1. Run `lookup_scan(&trace_id, &projection)` on every file concurrently (`try_join_all`).
2. Concatenate the batches with `arrow::compute::concat_batches` over the file-schema projection.
3. Convert with `to_table_batch`.

### `trace_list`

**Columns.** `st`, `tid` and `sid` are the indexes of `start_time`, `trace_id` and `span_id`; a missing one is `Schema`. The scan projection is `[st, tid, sid]` followed by the requested columns, so the request may repeat a column.

**Zones.** `fn time_zone(entry: &BlockEntry) -> (i64, i64)` reads `min` and `max` only when `entry.flags & crate::narrow::flags::HAS_ZONE != 0`: `entry.min.split_first_chunk::<8>()` mapped through `i64::from_le_bytes`, and the same for `max`. Otherwise, or if a split fails, it returns `(i64::MIN, i64::MAX)`. `BlockEntry` and `flags` live in `crate::narrow`; `predicate.rs` has an equivalent private helper, `zone_i64`. Make that one `pub(crate)` and reuse it instead of writing a second.

**Algorithm:**
1. **Candidate chunks.** For every file `f`, `block_entries(st)`. A chunk is a candidate when `max ≥ time.start && min < time.end`. Collect `(max, f, first_row, n_rows)` over all files, and sort by `max` descending, then file index and `first_row` ascending.
2. **Predicate.** `And([Cmp(st, GtEq, Int(time.start)), Cmp(st, Lt, Int(time.end))])`, plus `predicate` when given.
3. **Top-k.** Keep the `k` best rows in a `BinaryHeap<Reverse<ListKey>>`, a min-heap on the key, where `type ListKey = (i64, [u8; 16], [u8; 8], usize, usize)` is `(start_time, trace_id, span_id, batch index, row index)`. The batches are kept in a `Vec<RecordBatch>`.
   - `trace_id` and `span_id` are read with `as_fixed_size_binary().value(i)` and copied into arrays with `try_into()`. A width mismatch is `Corrupt { what: "trace_list key", offset: 0 }`.
4. **Chunk loop.** For each candidate chunk in order:
   - **Stop** if the heap holds `k` rows and the chunk's `max` is **strictly below** the heap minimum's `start_time`. Equal times still continue, because of the tie-break.
   - Otherwise scan its rows: `scan(&ScanRequest { projection, predicate, row_ranges: Some(vec![first_row..first_row + n_rows]), limit: None, kind: ScanKind::Scan })` on its file.
   - Push each output row into the heap. Pop the minimum while `len > k`.
5. **Output.**
   - Pop all rows and sort them by the key descending.
   - Build each output column with `arrow::compute::interleave` over the kept batches' columns. The requested column `j` is at position `3 + j` in every kept batch.
   - Make the batch over the file-schema projection, then convert with `to_table_batch`.
6. **Empty results.** The names and files are validated first. Then `k == 0` returns an empty batch with no I/O. An empty heap after the loop also returns `RecordBatch::new_empty`, before any `interleave`, which errors on an empty list.

**Doc comment on `trace_list`.** "Each visited chunk costs at least one wave. The early stop only helps when `start_time` zones are narrow, that is, when the file is sorted by time." Add the same note to § S9.

### Design doc (§ S9)

Update the signatures to `&[Arc<StrataFile>]`. Update the algorithm text to this plan's chunk-by-chunk scan, which fetches the projection with each chunk instead of fetching it for the final k rows. Add the note above.

### Tests (`tests/fast_paths.rs`, one `#[ignore]` test `fast_paths_match_datafusion`, run in release)

It reuses `tests/common/dataset.rs`: `spans_dataset(2_000, 3)`, written as Strata files into `InMemory`, plus a `StrataTable` over the same `Arc<StrataFile>`s registered as `t`.

- **`trace_detail`.** For each of the 100 ids, with `projection = ["span_id", "parent_span_id", "name", "start_time", "end_time", "status", "model", "total_tokens", "cost_total", "input"]`, compare with `SELECT <same columns> FROM t WHERE trace_id = X'<id>'`, as sorted row lists (M5.1's `rows`).
- **`trace_list`.**
  - For 10 one-hour windows starting at `t0 + 8h`, `t0 + 9h`, …, `t0 + 17h`, with the predicate `Cmp(span_kind, Eq, Utf8("llm"))`, `k = 50` and `projection = ["trace_id", "span_id", "name", "start_time", "duration_ns", "status", "model"]`, compare with `SELECT … WHERE span_kind = 'llm' AND start_time >= to_timestamp_nanos(a) AND start_time < to_timestamp_nanos(b) ORDER BY start_time DESC, trace_id DESC, span_id DESC LIMIT 50`, in order.
  - The predicate is `Cmp(schema.index_of("span_kind"), Eq, Utf8("llm"))`, with the index, not the name.
  - Also `k = 0`.
  - Also a window with no span, which gives an empty batch.
- **`trace_list` early stop.** The files above are sorted by `trace_id`, so every `start_time` zone is wide and the early stop never fires. Add a second set of files:
  - the same spans, sorted by `(start_time, trace_id, span_id)` and written with sort key `["start_time", "trace_id", "span_id"]` and `Config { chunk_max_rows: 256, ..Config::default() }`;
  - registered as table `t2` with their own `StrataTable`.

  Then:
  - compare `trace_list` with DataFusion over `t2` for the same 10 windows;
  - assert the early stop: for the window `t0 + 12h`, the summed `io_stats().requests` over the files after `trace_list` with `k = 50` is below the sum after `k = 1_000_000`. Call `reset_io_stats` before each run.

  `trace_detail` is not run on these files, because `lookup_scan` needs a FixedSizeBinary sort key.
- **Errors.** An unknown column name gives `Schema`.

Share the `rows` helper through `tests/common/dataset.rs`: move it there in this sub-phase, and update `tests/datafusion.rs` to import it.

### Done when

The M5.1 commands pass, with `fast_paths` added to the release command.

**Commit:** `strata: add trace_detail and trace_list fast paths (M5.2)`
