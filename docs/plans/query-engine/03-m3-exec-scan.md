# QE · M3 — Push-pipeline executor (`exec/`), `ScanSource`, `ParquetSource`, `TracesSource`, and `ScoresSource`

Milestone 3 of track QE ([`design.md`](design.md)). It builds the DuckDB-style synchronous push-pipeline execution engine inside `crates/query/src/exec/` (bounded process-wide CPU worker pool with 8 MiB stacks, fair multi-query round-robin scheduling, spillable `Sort`, `HashAgg`, `HashJoin`/`SemiJoin`/`AntiJoin`, `Window`, `TopK`, `Unnest`, `Union`, `Limit`, `Filter`, `Project`, `BatchSource`, and `DDSketch` approximate percentiles) and the storage scan & view-source layer inside `crates/storage/src/engine/` (`AsyncAdmissionPool`, bounded LRU `MetadataCache`, zero-Arrow low-level `ParquetSource` with bloom/page/min-max pruning + late materialization + corrupt-file `unwind`/`Suspects`, `MemtableSource`, native spillable `TraceMerger`/`TracesSource` with widening-window + two-phase rank→fetch, and native `ScoresSource` LWW dedup).

When M3 is done:
- `crates/query` remains 100% pure Rust with **zero `tokio`, `arrow`, `parquet`, `datafusion`, or `object_store`** dependencies, providing the complete push-pipeline executor (`exec/`) and format-agnostic `ScanSource`/`ScanSpec`/`BatchSource` contract;
- Every buffering operator (`Sort`, `HashAgg`, `HashJoin`, `TraceMerger`) spills to `<data_dir>/tmp/query/` via `cetra_column::frame` (LZ4 + CRC-32C) under `FairSpill`, respecting `Disk`'s frozen `min_free_bytes` spill cap and bounded open spill FDs;
- `cetra-storage::engine` implements `AsyncAdmissionPool` (`503 busy`), `MetadataCache` (`storage.query.metadata_cache_bytes`), zero-Arrow low-level `ParquetSource` (over `Arc<dyn ObjectStore>` + `CacheStore` + `ParquetMetaDataPushDecoder` + `Sbbf` + `SerializedPageReader` + `unwind`/`Suspects`), `MemtableSource`, `TracesSource` (exact 30-column rollup + widening-window + two-phase rank→fetch), and `ScoresSource` (`(updated_at DESC, ingest_time DESC)` LWW);
- Spill-under-tight-memory tests, `503 busy` admission tests, sub-millisecond cancel+drain tests, corrupt-file `Suspects` tests (including the 8-byte fuzz regression), and differential parity tests against DataFusion for `ParquetSource`, `TracesSource`, and `ScoresSource` all pass, and `just check` is green.

## Prerequisites

- Milestone 1 (`cetra-column` + `column_bridge`) and Milestone 2 (`cetra-query` `expr/`, `filter/`, `tombstone/`, `bind/`) are merged on branch `qe-query-engine`.
- Track QE design doc ([`design.md`](design.md)) is approved on `qe-query-engine`.

## Read first

1. `AGENTS.md` and `docs/plans/query-engine/design.md` (§ Contracts 1, 2, 3, 6; decisions `D-QE-2`, `D-QE-3`, `D-QE-5`, `D-QE-6`, `D-QE-13`; § Milestones → M3).
2. `crates/column/src/{memory.rs,cancel.rs,frame.rs,column.rs,batch.rs,selection.rs}`.
3. `crates/query/src/{lib.rs,error.rs,ir.rs,eval.rs,tombstone.rs}`.
4. `crates/storage/src/{config.rs,memory.rs,disk.rs,read_cache.rs,objects.rs,parquet.rs,unwind.rs,verify.rs,partials.rs,deletion.rs,column_bridge.rs}`.
5. `crates/storage/src/query/{mod.rs,traces.rs,trace_list.rs,scores.rs}` — exact pruning, widening-window, tombstone grouping, and LWW dedup invariants.

## What it is, and what it is not

**It is:**
- zero-copy consuming selection/projection methods on `cetra_column::Batch`;
- the `cetra-query::exec` push-pipeline engine, bounded CPU worker pool, spill manager, shared `SortKey` comparator, and all physical operators (`Filter`, `Project`, `Limit`, `TopK`, `Union`, `Unnest`, `Sort`, `Window`, `HashAgg`, `HashJoin`/`SemiJoin`/`AntiJoin`/`CrossJoin`);
- the `cetra-storage::engine` runtime and scan sources (`AsyncAdmissionPool`, `MetadataCache`, `ParquetSource`, `MemtableSource`, `TracesSource`, `ScoresSource`), plus differential, spill, admission, cancel-drain, and corrupt-file tests.

**It is not:**
- porting `crates/storage/src/reads/` (M4);
- Shape B SQL statement planner or JSON/CSV/NDJSON/Parquet encoders (M5);
- migrating compaction, deletion rewriter, media GC, WAL, or memtable internals off Arrow/DataFusion (M6);
- changing any public `cetra-core` or `cetra-storage` API signature (M7).

## Versions

M3 adds **zero** new external crates.
- `crates/query/Cargo.toml`: keeps its exact M2 dependencies (adds `tempfile.workspace = true` under `[dev-dependencies]` for spill tests). Zero `tokio`, `arrow`, `parquet`, `datafusion`, `object_store`.
- `crates/storage/Cargo.toml`: promotes `cetra-query.workspace = true` from `[dev-dependencies]` to `[dependencies]` in `QE3.6` (no new external crate).

## Phases and dependency graph

```text
QE3.1 (Batch zero-copy ops, QueryError::Busy/Internal, exec core: WorkerPool, SpillManager,
       SortKey, ScanSource/ScanSpec/BatchSource, Pipeline runtime, ExecMetrics, Expr column-remap helpers)
  ├── QE3.2 (streaming ops: Filter, Project, Limit, Union, Unnest, TopK)                       ┐
  ├── QE3.3 (sort_window: spillable external Sort + Window functions)                           │
  ├── QE3.4 (hash_agg: spillable HashAgg + DDSketch + exact percentile + sessions aggs)         ├── 5 PARALLEL PHASES
  ├── QE3.5 (hash_join: spillable Grace HashJoin / SemiJoin / AntiJoin / CrossJoin)             │
  └── QE3.6 (storage::engine: AsyncAdmissionPool, Disk cap, MetadataCache, ParquetSource,       │
             MemtableSource, Suspects::report_corrupt)                                          ┘
        └── QE3.7 (storage::engine: native TraceMerger/TracesSource, ScoresSource, differential + fault tests)
              └── QE3.8 (Neon code review, fixes, docs/roadmap.md, full just check)
```

`QE3.2`–`QE3.5` each create one disjoint module inside `crates/query/src/exec/` (adding their single `pub mod ...;` line in `crates/query/src/exec/mod.rs`, merged mechanically by the coordinator before `QE3.7`). `QE3.6` edits `crates/storage/` only, so it runs in full parallel with `QE3.2`–`QE3.5`.

| Phase | Edits | Depends on |
|---|---|---|
| QE3.1 Zero-copy `Batch` helpers, `QueryError` variants, push-pipeline core, `WorkerPool`, `SpillManager`, `SortKey`, `ScanSource`/`BatchSource` | `crates/column/src/batch.rs`, `crates/query/Cargo.toml`, `crates/query/src/{lib.rs,error.rs,ir.rs,exec/{mod.rs,pool.rs,spill.rs,sort_key.rs,source.rs,pipeline.rs}}` | M2 |
| QE3.2 Streaming operators & `TopK` | `crates/query/src/exec/{mod.rs,streaming.rs}` | QE3.1 |
| QE3.3 Spillable external `Sort` & `Window` | `crates/query/src/exec/{mod.rs,sort_window.rs}` | QE3.1 |
| QE3.4 Spillable `HashAgg`, `DDSketch`, and `sessions` aggregates | `crates/query/src/exec/{mod.rs,hash_agg.rs}` | QE3.1 |
| QE3.5 Spillable `HashJoin`, `SemiJoin`, `AntiJoin`, `CrossJoin` | `crates/query/src/exec/{mod.rs,hash_join.rs}` | QE3.1 |
| QE3.6 Storage `AsyncAdmissionPool`, `Disk` spill cap, `MetadataCache`, zero-Arrow `ParquetSource`, `MemtableSource`, `Suspects::report_corrupt` | `crates/storage/Cargo.toml`, `crates/storage/src/{lib.rs,disk.rs,verify.rs,engine/{mod.rs,admission.rs,metadata_cache.rs,parquet_source.rs,mem_source.rs}}`, `Cargo.lock` | QE3.1 |
| QE3.7 Native `TracesSource` (`TraceMerger` + widening-window + two-phase), `ScoresSource` LWW, differential & fault tests | `crates/storage/src/engine/{mod.rs,traces_source.rs,scores_source.rs,diff_tests.rs}` | QE3.2–QE3.6 |
| QE3.8 Neon code review, fixes, and M3 closeout | fixes + `docs/roadmap.md` | QE3.7 |

## Cross-cutting invariants for every phase

1. **Safe Rust, lints & privacy:** `#![forbid(unsafe_code)]`, `#![warn(missing_docs)]`, `missing_debug_implementations = "warn"`, no `unwrap()`/`expect()` outside tests (unless an adjacent invariant comment proves infallibility), zero `#![allow(...)]` on library code. Every `pub` struct/enum implements `Debug`, and any struct holding user strings or search needles (`IdLookupHint`, `SubstringHint`, `SpillManager`) uses **manual `fmt::Debug`** printing column names and counts/lengths only (never payload values or erased user/session IDs!).
2. **Crate purity:** `cetra-query` MUST NEVER depend on `tokio`, `arrow`, `parquet`, `datafusion`, or `object_store`.
3. **No detached workers past drop (`design.md` § Contracts 3):** Every `WorkerPool::run_scoped` call cancels `CancelToken` on first error and **synchronously joins/drains** all in-flight worker tasks for that query before returning (even on error or panic), guaranteeing zero leaked `Reservation`s and sub-millisecond cancellation latency. `Drop for WorkerPool` signals shutdown and joins all worker threads cleanly so tests never leak 8 MiB-stack OS threads.
4. **Spill safety & FD bounds:**
   - All temporary spill files are unique files inside `<data_dir>/tmp/query/` (created by `SpillManager`, which shares one process-wide `Arc<AtomicU64>` `spilled_bytes` counter and `SpillCapFn`), encoded as `cetra_column::frame::encode_batch` (LZ4 + CRC-32C), and deleted automatically on `Drop` of either `SpillWriter` (if dropped uncommitted) or `SpillFile` — subtracting their bytes from `spilled_bytes` even on error or panic!
   - Before writing a spill frame, `SpillManager` checks `spilled_bytes + frame_len <= (cap)()`, where `cap` reads `Disk`'s frozen `engine_spill_cap: AtomicU64` updated at `statvfs` measurement time (`spilled_at_statvfs + (free - min_free)`). Maps `ENOSPC` (`io::ErrorKind::StorageFull`), `EDQUOT` (`io::ErrorKind::QuotaExceeded`), and cap-exceeded to `QueryError::TooLarge`; other spill I/O errors map to `QueryError::Internal(...)`.
   - Every k-way merge bounds simultaneously open spill files to `max_merge_way` (default `MAX_MERGE_WAY = 32`); if more than `max_merge_way` runs exist, merge runs in passes into intermediate `SpillFile`s so open FDs stay `<= max_merge_way + 1` regardless of spill size.
5. **Exact `Reservation` handoff & `FairSpill` spilling:**
   - **Per-batch handoff inside `run_pipeline`:**
     - `run_pipeline` holds one unspillable `scan_res` per partition when calling `stream.next_batch(&cancel, Some(&mut scan_res))` (postcondition: `scan_res.size() == batch.memory_bytes()` on `Ok(Some(batch))`, `0` on `Ok(None)` / `Err`).
     - Before pushing `batch` into the operator chain, `run_pipeline` releases `scan_res` only **after** `push` returns (or immediately before a buffering `PushSink::consume`? Better and zero double-counting: `PushOperator`s allocate their own scratch/output `Reservation` during `push`, call `emit(out)`, and `free()` their scratch `Reservation` immediately after `emit(out)` returns; `run_pipeline` passes owned `batch` into the chain and calls `scan_res.free()` on `push` return and on `Err`/`Stop`. For buffering sinks (`ExternalSort`, `HashAgg`, `HashJoin`), `scan_res` is freed on `run_pipeline` after `consume` returns — and if a sink hits `MemoryLimit` while `buffered_rows > 0`, it spills its already-buffered rows first so the incoming batch fits!).
   - **Spill trigger:** Every buffering operator registers a spillable `Reservation` (`pool.register(name, true)`) and spills on **any** `ColumnError::MemoryLimit { .. }` (`SpillShare`, `QueryCap`, **or `GlobalPool`**!) iff `buffered_rows > 0`. If `try_grow` still fails after spilling to `0` buffered rows, return `Err(QueryError::TooLarge)` immediately (never loop!).
   - **Merge frame sizing:** Spilled sorted runs write frames sliced to `<= max(spill_share / (max_merge_way + 2), 4096)` bytes (at least 1 row per frame) so holding `max_merge_way` active merge batches simultaneously fits inside a tight `ScopedPool` (e.g. `64 KiB` in tests!).
6. **Total-order sorting & `NULLS FIRST`/`LAST`:**
   - Every sort, `TopK`, `Window`, and key comparison uses `cetra_query::exec::SortKey` + `compare_rows` / `compare_rows_across` with `eval::f64_cmp` (`-0.0 == +0.0`, `NaN == NaN`, `NaN > +inf`) and explicit `nulls_first: bool`.
7. **Zero-copy `Selection` & `ScanSource` filter exactness:**
   - Default batch size is `DEFAULT_BATCH_SIZE = 8192` rows.
   - `FilterOp` and `LimitOp` use consuming zero-copy `Batch::into_filter_mask` / `Batch::into_with_selection` (never cloning column `Vec` buffers!). Physical compaction (`Batch::compact`) runs before spilling, sorting, hashing, or when `active * 4 < raw_len`.
   - **Every `ScanSource`** evaluates `spec.filters` (bound against `self.schema()`) **exactly** before projecting to `self.projected_schema(spec)` (including empty `projection = Some(vec![])` for `SELECT COUNT(*)`, returning a 0-column `Batch` with exact `raw_len` / `Selection`!).
8. **Machine load:** While parallel agents run, each runs only touched-crate tests + clippy + doc + fmt once right before committing.

---

## QE3.1 — Zero-copy `Batch` helpers, `QueryError` variants, push-pipeline core, `WorkerPool`, `SpillManager`, `SortKey`, and `ScanSource`/`BatchSource`

**Goal:** Add zero-copy consuming selection/projection methods on `cetra_column::Batch`, add `QueryError::Busy` and `QueryError::Internal(String)`, add `Expr::referenced_columns` / `Expr::remap_columns`, and build the foundational `crates/query/src/exec/` runtime (`WorkerPool`, `SpillManager`, `SortKey`, `ScanSource`/`ScanSpec`/`BatchSource`, `PushOperator`, `PushSink`, `run_pipeline`, `ExecMetrics`).

**Edit:**
- `crates/column/src/batch.rs`: add zero-copy consuming methods `pub fn into_with_selection(mut self, selection: Selection) -> Result<Self, ColumnError>`, `pub fn into_filter_mask(self, mask: &Bitmap) -> Result<Self, ColumnError>`, `pub fn into_project(mut self, indices: &[usize]) -> Result<Self, ColumnError>` (moving unique columns without cloning and cloning only on duplicate indices, since `Schema::project` permits duplicates like `&[0, 0]`).
- `crates/query/Cargo.toml`: add `tempfile.workspace = true` under `[dev-dependencies]`.
- `crates/query/src/error.rs`: add `QueryError::Busy` (`503 busy`) and `QueryError::Internal(String)` (`500`, capped/redacted diagnostic, plus `QueryError::internal(msg)`).
- `crates/query/src/ir.rs`: add `Expr::referenced_columns(&self, out: &mut BTreeSet<usize>)` and `Expr::remap_columns(&self, map: &[Option<usize>]) -> Result<Expr, QueryError>` (for projection/filter remapping in `ScanSource` and two-phase `TracesSource`).
- `crates/query/src/lib.rs`: add `pub mod exec;`.
- `crates/query/src/exec/{mod.rs,pool.rs,spill.rs,sort_key.rs,source.rs,pipeline.rs}` (new).

### Contracts

`crates/query/src/exec/pool.rs`:
```rust
/// Process-wide bounded CPU worker pool with 8 MiB worker stacks and fair round-robin
/// scheduling across concurrent queries (`D-QE-5`, `design.md` § Contracts 3).
#[derive(Debug)]
pub struct WorkerPool { /* fixed OS threads with 8 MiB stacks, fair per-query work queues */ }

impl WorkerPool {
    /// Creates a worker pool with `num_workers.max(1)` OS threads, each with an 8 MiB stack
    /// (`std::thread::Builder::new().name(...).stack_size(8 * 1024 * 1024)`).
    pub fn new(num_workers: usize) -> Result<Self, QueryError>;

    /// Returns the number of CPU worker threads in this pool.
    pub fn num_workers(&self) -> usize;

    /// Executes `tasks` in parallel on the worker pool under fair round-robin scheduling,
    /// returning results in **original task index order** (empty `tasks` returns `Ok(Vec::new())` immediately).
    ///
    /// On first error (or cancellation), drains unstarted tasks, calls `cancel.cancel()`, and
    /// **synchronously blocks until all in-flight tasks finish** before returning.
    /// Panics inside tasks are caught via `std::panic::catch_unwind` and mapped to
    /// `QueryError::internal("query worker panicked")` without killing or poisoning worker threads.
    pub fn run_scoped<F, T>(
        &self,
        cancel: &CancelToken,
        tasks: Vec<F>,
    ) -> Result<Vec<T>, QueryError>
    where
        F: FnOnce(usize) -> Result<T, QueryError> + Send + 'static,
        T: Send + 'static;
}
// `Drop for WorkerPool` signals shutdown on the shared Condvar and joins all worker threads.
```

`crates/query/src/exec/spill.rs`:
```rust
/// Default maximum simultaneously open spill files during a k-way merge pass.
pub const MAX_MERGE_WAY: usize = 32;

/// Callback returning the frozen process-wide maximum total spilled bytes allowed right now
/// (wired to `Disk::engine_spill_cap()` in `cetra-storage`).
pub type SpillCapFn = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Manages unique temporary spill files inside a root directory (`<data_dir>/tmp/query`).
/// Implements manual `fmt::Debug` (counters only).
#[derive(Clone)]
pub struct SpillManager { /* root: PathBuf, next_id: Arc<AtomicU64>, spilled_bytes: Arc<AtomicU64>, cap: SpillCapFn */ }

impl SpillManager {
    /// Creates a spill manager rooted at `dir` with a dynamic byte cap callback.
    pub fn new(dir: PathBuf, cap: SpillCapFn) -> Self;
    /// Convenience constructor with a fixed byte cap (for unit tests).
    pub fn with_fixed_cap(dir: PathBuf, max_bytes: u64) -> Self;
    /// Shared live spilled-bytes counter (passed to `Disk` so `apply_spill_cap` reads exact live bytes).
    pub fn spilled_bytes_counter(&self) -> Arc<AtomicU64>;
    /// Current live spilled bytes across all active spill writers and files.
    pub fn spilled_bytes(&self) -> u64;
    /// Opens a new unique spill writer inside `dir` (deleted + bytes subtracted on drop if uncommitted).
    pub fn create_file(&self) -> Result<SpillWriter, QueryError>;
}

/// Streaming writer appending `cetra_column::Batch`es as LZ4+CRC-32C frames (`cetra_column::frame`).
#[derive(Debug)]
pub struct SpillWriter { /* ... */ }
impl SpillWriter {
    /// Writes `batch` (compacting `Selection` if non-`All`), checking `spilled_bytes + frame_len <= (cap)()`
    /// before writing. Maps `StorageFull`/`QuotaExceeded`/cap-exceeded to `QueryError::TooLarge`
    /// and other I/O errors to `QueryError::Internal(...)`.
    pub fn write_batch(&mut self, batch: &Batch) -> Result<(), QueryError>;
    /// Flushes and converts into a readable [`SpillFile`] (auto-deleted on `SpillFile::drop`).
    pub fn finish(self) -> Result<SpillFile, QueryError>;
}

/// Handle to a completed temporary spill file; deletes the file and subtracts its bytes from
/// `SpillManager::spilled_bytes()` on `Drop`.
#[derive(Debug)]
pub struct SpillFile { /* ... */ }
impl SpillFile {
    /// Bytes on disk.
    pub fn byte_len(&self) -> u64;
    /// Opens a sequential frame reader validating against `schema`.
    pub fn open_reader(&self, schema: Arc<Schema>) -> Result<SpillReader, QueryError>;
}

/// Sequential frame reader yielding `Option<Batch>` from a [`SpillFile`].
#[derive(Debug)]
pub struct SpillReader { /* ... */ }
impl SpillReader {
    /// Reads the next batch. `Reservation` postcondition: `res.size() == before + batch.memory_bytes()`
    /// on `Ok(Some(batch))`, and restored to `before` on `Ok(None)` or `Err`.
    pub fn next_batch(&mut self, res: Option<&mut Reservation>) -> Result<Option<Batch>, QueryError>;
}
```

`crates/query/src/exec/sort_key.rs`:
```rust
/// Sort specification shared by `TopK`, `ExternalSort`, `Window`, `FirstNonNullByOrder`, and `TracesSource`/`ScoresSource`.
#[derive(Debug, Clone, PartialEq)]
pub struct SortKey {
    /// Expression producing the sort key column.
    pub expr: Expr,
    /// True for `DESC`, false for `ASC`.
    pub descending: bool,
    /// Explicit null placement (`true` = `NULLS FIRST`, `false` = `NULLS LAST`).
    pub nulls_first: bool,
}

/// Compares row `a` and row `b` within the same evaluated `key_cols` slice (using `eval::f64_cmp`
/// for `F64` and explicit `nulls_first`, supporting all sortable `DataType`s across all 4 `Column` encodings).
pub fn compare_rows(key_cols: &[Column], keys: &[SortKey], a: usize, b: usize) -> std::cmp::Ordering;

/// Compares row `a` in `cols_a` against row `b` in `cols_b` (used by k-way merge and `TopKSink::merge`).
pub fn compare_rows_across(
    cols_a: &[Column],
    a: usize,
    cols_b: &[Column],
    b: usize,
    keys: &[SortKey],
) -> std::cmp::Ordering;
```

`crates/query/src/exec/source.rs`:
```rust
/// Inclusive UTC-nanosecond range hint extracted for file/row-group pruning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TimeRangeHint {
    /// Inclusive lower bound (`>= min_ns`).
    pub min_ns: Option<i64>,
    /// Inclusive upper bound (`<= max_ns`).
    pub max_ns: Option<i64>,
    /// Optional `QueryOptions::around` anchor hint (±1 day pruning for spans/traces lookups, D154).
    pub around_ns: Option<i64>,
}

/// Binary or string ID equality / `IN (...)` lookup hint for bloom-filter and sort-key pruning.
/// Manual `fmt::Debug` prints column + value count only (never user/session IDs, `AGENTS.md` § Privacy).
#[derive(Clone, PartialEq, Eq)]
pub enum IdLookupHint {
    /// 16-byte binary IDs (e.g. `trace_id`).
    Fixed16 { column: String, values: Vec<[u8; 16]> },
    /// 8-byte binary IDs (e.g. `span_id`).
    Fixed8 { column: String, values: Vec<[u8; 8]> },
    /// String IDs (e.g. `session_id`, `user_id`, `score_id`).
    Utf8 { column: String, values: Vec<String> },
}

/// Substring search hint (`col LIKE/ILIKE '%needle%'`).
/// Manual `fmt::Debug` prints column + needle byte length + case flag only (never needle text!).
#[derive(Clone, PartialEq, Eq)]
pub struct SubstringHint {
    /// Column name.
    pub column: String,
    /// Unescaped literal needle.
    pub needle: String,
    /// True for `ILIKE`.
    pub case_insensitive: bool,
}

/// Requested output sort hint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortHint {
    /// Column name.
    pub column: String,
    /// True for `DESC`.
    pub descending: bool,
}

/// Format-agnostic scan specification passed to every [`ScanSource`] (`D-QE-3`).
#[derive(Debug, Clone, Default)]
pub struct ScanSpec {
    /// Projected column indices in the source's full schema (`None` = all columns; `Some(vec![])` = 0-column row-count batch).
    pub projection: Option<Vec<usize>>,
    /// Referenced map keys `(column_name, key)` for shredded readers (Strata).
    pub map_keys: Vec<(String, String)>,
    /// Time range hint on the table's primary time column (`start_time` or `created_at`).
    pub time_range: TimeRangeHint,
    /// ID lookup hints (`trace_id`, `span_id`, `session_id`, `user_id`, `score_id`).
    pub id_lookups: Vec<IdLookupHint>,
    /// Conjunctive predicates bound against the source's **full** `schema()`; every `ScanSource` MUST evaluate these exactly before projecting.
    pub filters: Vec<Expr>,
    /// Substring search hints.
    pub substrings: Vec<SubstringHint>,
    /// Optional requested ordering hints.
    pub sort_hints: Vec<SortHint>,
    /// Optional row limit hint (sources may still emit more; upstream `Limit` enforces exactness).
    pub limit: Option<usize>,
}

/// One partition stream pulled synchronously by a CPU worker inside a push pipeline.
pub trait PartitionStream: Send {
    /// Returns the next `Batch` (or `Ok(None)` when exhausted), checking `cancel`.
    /// `Reservation` postcondition: `res.size() == before + batch.memory_bytes()` on `Ok(Some)`,
    /// and restored to `before` on `Ok(None)` or `Err`.
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError>;
}

/// Format-agnostic table/file scan boundary (`ParquetSource`, future `StrataSource`,
/// `MemtableSource`, `TracesSource`, `ScoresSource`, and in-memory `BatchSource`).
pub trait ScanSource: Send + Sync + std::fmt::Debug {
    /// Full schema of this source (before `ScanSpec::projection`).
    fn schema(&self) -> Arc<Schema>;
    /// Output schema after applying `spec.projection`.
    fn projected_schema(&self, spec: &ScanSpec) -> Result<Arc<Schema>, QueryError>;
    /// Opens up to `ctx.target_partitions` synchronous partition streams for `spec`.
    /// Every emitted batch MUST have passed all `spec.filters` and match `projected_schema(spec)`.
    fn open_partitions(
        &self,
        spec: &ScanSpec,
        ctx: &ExecContext,
    ) -> Result<Vec<Box<dyn PartitionStream>>, QueryError>;
    /// Human-readable plan marker (e.g. `"mode=list"` / `"mode=range"` / `"mode=lookup"`).
    fn explain_summary(&self, _spec: &ScanSpec) -> String;
}

/// In-memory [`ScanSource`] backed by pre-built `cetra_column::Batch`es (evaluates `spec.filters`
/// exactly and supports `projection = Some(vec![])`).
#[derive(Debug, Clone)]
pub struct BatchSource { /* schema: Arc<Schema>, batches: Arc<Vec<Batch>> */ }
```

`crates/query/src/exec/pipeline.rs`:
```rust
/// Default target rows per batch across operators (`views/session.rs:41`).
pub const DEFAULT_BATCH_SIZE: usize = 8192;

/// Flow-control signal returned by push operators and sinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlFlow {
    /// Continue pushing more batches.
    Continue,
    /// Downstream is satisfied (e.g. `LIMIT` reached); stop reading this partition immediately.
    Stop,
}

/// Per-operator execution profile (powers `trace_bench` missed-target diagnostics in M4).
#[derive(Debug, Clone, Default)]
pub struct OperatorProfile {
    /// Operator display name.
    pub name: String,
    /// Rows emitted.
    pub rows_out: u64,
    /// Batches emitted.
    pub batches_out: u64,
    /// Wall time spent inside this operator (nanoseconds).
    pub elapsed_ns: u64,
}

/// Thread-safe accumulator collecting [`OperatorProfile`] snapshots across workers.
#[derive(Debug, Default, Clone)]
pub struct ExecMetrics { /* Arc<Mutex<Vec<OperatorProfile>>> */ }

/// Shared per-query execution context passed to every pipeline and operator.
#[derive(Debug, Clone)]
pub struct ExecContext {
    /// Query-scoped memory pool (`FairSpill` + query cap).
    pub pool: Arc<ScopedPool>,
    /// Evaluation context (`now_ns`, `CancelToken`).
    pub eval: EvalContext,
    /// Spill manager for out-of-core buffering operators.
    pub spill: Arc<SpillManager>,
    /// Bounded process-wide CPU worker pool.
    pub workers: Arc<WorkerPool>,
    /// Target rows per batch (`DEFAULT_BATCH_SIZE`).
    pub batch_size: usize,
    /// Maximum parallel partitions for this query (`storage.query.target_partitions`).
    pub target_partitions: usize,
    /// Per-operator profile recorder.
    pub metrics: ExecMetrics,
}

/// Push-based stateless/stateful pipeline operator transforming one `Batch` into zero or more `Batch`es.
pub trait PushOperator: Send {
    /// Operator name for `EXPLAIN` and [`ExecMetrics`].
    fn name(&self) -> &'static str;
    /// Processes `batch` and forwards zero or more output batches to `emit`.
    /// Scratch `Reservation`s held for an output batch MUST be freed right after `emit(out)` returns.
    fn push(
        &mut self,
        batch: Batch,
        emit: &mut dyn FnMut(Batch) -> Result<ControlFlow, QueryError>,
    ) -> Result<ControlFlow, QueryError>;
    /// Called once when upstream finishes cleanly (not cancelled/stopped) to flush buffered output.
    fn finish(
        &mut self,
        emit: &mut dyn FnMut(Batch) -> Result<ControlFlow, QueryError>,
    ) -> Result<(), QueryError> {
        let _ = emit;
        Ok(())
    }
}

/// Terminal consumer of one pipeline partition.
pub trait PushSink: Send {
    /// Consumes one batch; returning `ControlFlow::Stop` terminates the partition early.
    fn consume(&mut self, batch: Batch) -> Result<ControlFlow, QueryError>;
    /// Called once when the partition finishes cleanly.
    fn finish(&mut self) -> Result<(), QueryError> { Ok(()) }
}

/// Runs `partitions` in parallel on `ctx.workers`, pushing each partition through a fresh
/// operator chain built by `make_chain(partition_idx)` into `make_sink(partition_idx)`.
/// Frees each partition's `scan_res` immediately after pushing each batch (and on error/stop),
/// and synchronously drains all workers before returning finalized sinks in partition index order.
pub fn run_pipeline<S, FChain, FSink>(
    ctx: &ExecContext,
    partitions: Vec<Box<dyn PartitionStream>>,
    make_chain: FChain,
    make_sink: FSink,
) -> Result<Vec<S>, QueryError>
where
    S: PushSink + Send + 'static,
    FChain: Fn(usize) -> Result<Vec<Box<dyn PushOperator>>, QueryError> + Send + Sync + 'static,
    FSink: Fn(usize) -> Result<S, QueryError> + Send + Sync + 'static;

/// Collects all batches emitted by one pipeline partition (tracked in `Reservation`).
#[derive(Debug)]
pub struct CollectSink { /* batches: Vec<Batch>, res: Reservation */ }
```

### Tests

- `crates/column`: unit tests for zero-copy `into_with_selection`, `into_filter_mask`, `into_project`.
- `exec::pool::tests::fair_round_robin_order_and_cancel_drains_all_workers` — verifies 8 MiB stack, task index ordering, fair scheduling, panic → `QueryError::Internal` without poisoning, `Drop` joins threads, and cancelling mid-flight drains all workers to `in_flight == 0` in `< 5 ms`.
- `exec::spill::tests::spill_roundtrip_cap_enforcement_and_cleanup_on_drop` — verifies LZ4+CRC-32C spill roundtrip, `SpillReader` `Reservation` postcondition, fixed cap overflow → `QueryError::TooLarge`, and automatic file deletion + `spilled_bytes() == 0` on both uncommitted `SpillWriter` drop and `SpillFile` drop.
- `exec::sort_key::tests::compare_rows_across_all_types_and_nulls_first_last`.
- `exec::source::tests::batch_source_filters_empty_projection_and_privacy_debug` — verifies exact filter evaluation, `projection = Some(vec![])`, and that `format!("{spec:?}")` hides `IdLookupHint::Utf8` values and `SubstringHint::needle`.
- `exec::pipeline::tests::early_stop_reservation_handoff_and_metrics`.

### Done when

- `cargo fmt --all && cargo test -p cetra-column -p cetra-query && cargo clippy -p cetra-column -p cetra-query --all-targets -- -D warnings && cargo doc -p cetra-column -p cetra-query --no-deps && cargo fmt --all -- --check` passes.
- `cargo tree -p cetra-query` confirms zero `tokio`, `arrow`, `parquet`, `datafusion`, `object_store`.

**Commit:** `query: add push-pipeline executor core, WorkerPool, SpillManager, SortKey, and ScanSource`

---

## QE3.2 — Streaming operators & `TopK` (`exec/streaming.rs`) *(parallel with QE3.3–QE3.6)*

**Goal:** Implement the streaming push operators (`FilterOp`, `ProjectOp`, `LimitOp`, `UnnestOp`, `UnionSource`) and bounded-memory `TopKSink` (using QE3.1's `SortKey` and zero-copy `Batch::into_*` methods).

**Edit:**
- `crates/query/src/exec/streaming.rs` (new).
- `crates/query/src/exec/mod.rs`: add `pub mod streaming;`.

### Contracts

`crates/query/src/exec/streaming.rs`:
- `FilterOp::new(predicate: Expr, ctx: ExecContext) -> Self`: evaluates `eval_filter`, applies zero-copy `batch.into_filter_mask(&mask)`, drops 0-row batches, compacts via `Batch::compact` only when `active * 4 < raw_len`.
- `ProjectOp::new(exprs: Vec<(String, Expr)>, ctx: ExecContext) -> Self`: evaluates expressions preserving `batch.selection()`, accounting output bytes in a scratch `Reservation` freed right after `emit` returns.
- `LimitOp::new(offset: usize, limit: Option<usize>, shared_remaining: Option<Arc<AtomicUsize>>) -> Self`:
  - Uses zero-copy `Batch::into_with_selection`; returns `ControlFlow::Stop` immediately when `limit == Some(0)` or reached.
  - When `shared_remaining` is `Some(rem)` (parallel unsorted `offset == 0`), uses `rem.fetch_update(...)` CAS to claim `take = batch_rows.min(current)` so concurrent partitions never overshoot `limit`!
- `UnnestOp::new(unnest_cols: Vec<usize>, preserve_nulls: bool, ctx: ExecContext) -> Self`:
  - Expands one or more `List(T)` columns with Postgres/DataFusion zipping semantics (row repeats `max(list_len)` times, shorter lists pad with `NULL`).
  - `preserve_nulls = true` (SQL `SELECT unnest(tags)` default): empty/null lists emit 1 row with `NULL` unnest column(s).
  - `preserve_nulls = false` (facets/widgets `NullHandling::Drop`): empty/null lists emit 0 rows.
- `UnionSource::new(schema: Arc<Schema>, sources: Vec<Arc<dyn ScanSource>>) -> Result<Self, QueryError>`: implements `ScanSource`, concatenating child partitions for `UNION ALL`.
- `TopKSink`:
  - `TopKSink::new(schema: Arc<Schema>, keys: Vec<SortKey>, limit: usize, offset: usize, ctx: ExecContext) -> Self`
  - Implements `PushSink` (immediately returns `ControlFlow::Stop` when `limit == 0`). Maintains a bounded binary heap of size `offset.saturating_add(limit)` using `compare_rows_across`, tracking kept-row bytes in an unspillable `Reservation`.
  - `TopKSink::merge(sinks: Vec<TopKSink>, schema: Arc<Schema>, keys: &[SortKey], offset: usize, limit: usize, ctx: &ExecContext) -> Result<Vec<Batch>, QueryError>` — merges per-partition heaps, skips `offset`, and emits sorted `Batch`es of at most `ctx.batch_size` rows.

### Tests

- `exec::streaming::tests::filter_project_limit_zero_copy_and_parallel_cas` — verifies zero-copy `Selection`, `limit == Some(0)`, `offset + limit`, parallel `shared_remaining` exact cap, and `ControlFlow::Stop`.
- `exec::streaming::tests::topk_total_order_nulls_first_last_and_multi_partition_merge`.
- `exec::streaming::tests::unnest_preserve_nulls_true_and_false_with_multi_list_zip`.

### Done when

- `cargo fmt --all && cargo test -p cetra-query && cargo clippy -p cetra-query --all-targets -- -D warnings && cargo doc -p cetra-query --no-deps && cargo fmt --all -- --check` passes.

**Commit:** `query: add streaming push operators (Filter, Project, Limit, Unnest, Union, TopK)`

---

## QE3.3 — Spillable external `Sort` & `Window` (`exec/sort_window.rs`) *(parallel with QE3.2, QE3.4–QE3.6)*

**Goal:** Implement external spillable `ExternalSort` (implementing `PushSink`, multi-pass bounded-FD k-way merge with sized spill frames, plus multi-stream `merge_sorted_streams`) and `WindowOp` (`OVER ([PARTITION BY ...] [ORDER BY ...] [ROWS BETWEEN ...])`).

**Edit:**
- `crates/query/src/exec/sort_window.rs` (new).
- `crates/query/src/exec/mod.rs`: add `pub mod sort_window;`.

### Contracts

`crates/query/src/exec/sort_window.rs`:
- `ExternalSort`:
  - `ExternalSort::new(schema: Arc<Schema>, keys: Vec<SortKey>, limit: Option<usize>, ctx: ExecContext) -> Self`
  - `pub fn with_max_merge_way(mut self, max_merge_way: usize) -> Self` (default `MAX_MERGE_WAY = 32`, clamped `>= 2`).
  - Implements `PushSink` (`consume` appends compacted rows; spills on **any** `ColumnError::MemoryLimit { .. }` iff `buffered_rows > 0`, writing sorted run frames sliced to `<= max(spill_share / (max_merge_way + 2), 4096)` bytes and at least 1 row per frame so k-way merge fits inside tight pools!).
  - `pub fn into_stream(self) -> Result<Box<dyn PartitionStream>, QueryError>`: merges in-memory + spilled runs with multi-pass merge when `runs.len() > max_merge_way` (bounding open FDs to `<= max_merge_way + 1`).
  - `pub fn merge_sinks(sinks: Vec<ExternalSort>, schema: Arc<Schema>, keys: Vec<SortKey>, limit: Option<usize>, ctx: &ExecContext) -> Result<Box<dyn PartitionStream>, QueryError>` — combines multiple per-partition `ExternalSort` sinks into one globally sorted `PartitionStream` (used by `TracesSource`, `ScoresSource`, and SQL `ORDER BY`).
- `WindowOp`:
  - `pub enum WindowFunc { RowNumber, Rank, DenseRank, Lag { expr: Expr, offset: i64, default: Option<Expr> }, Lead { expr: Expr, offset: i64, default: Option<Expr> }, FirstValue(Expr), LastValue(Expr), Sum(Expr), Count(Option<Expr>), Avg(Expr), Min(Expr), Max(Expr) }`
  - `pub enum FrameBound { UnboundedPreceding, Preceding(u64), CurrentRow, Following(u64), UnboundedFollowing }`
  - `pub struct WindowSpec { pub partition_by: Vec<Expr>, pub order_by: Vec<SortKey>, pub frame: Option<(FrameBound, FrameBound)>, pub func: WindowFunc, pub output_name: String }`
  - `pub fn eval_window_group(input: Box<dyn PartitionStream>, input_schema: Arc<Schema>, partition_by: Vec<Expr>, order_by: Vec<SortKey>, specs: Vec<WindowSpec>, ctx: &ExecContext) -> Result<Box<dyn PartitionStream>, QueryError>`:
    - Output schema = `input_schema` extended with each `spec.output_name` column.
    - 0 input rows → 0 output rows.
    - Sorts input via `ExternalSort` on `(partition_by..., order_by...)`, streams one partition at a time into RAM (bounded by `Reservation`), evaluates all `specs`:
      - Default frame when `frame == None`: whole partition if `order_by.is_empty()`; running prefix through current `order_by` peer group if `order_by` is non-empty (applies to `Sum`/`Count`/`Avg`/`Min`/`Max` **and** `FirstValue`/`LastValue`, matching Postgres/DataFusion!).
      - Explicit `ROWS BETWEEN a AND b` uses exact row offsets.
      - `Lag`/`Lead`: `offset == 0` returns current row; negative `offset` flips direction (`Lag(-k) == Lead(k)`).

### Tests

- `exec::sort_window::tests::external_sort_spills_under_tight_memory_and_bounds_fds` — forces 40+ runs with `with_max_merge_way(4)` (multi-pass merge!) under a `64 KiB` `ScopedPool`, asserting exact sorted output, `spilled_bytes() == 0` after drop, and `pool.reserved() == 0`.
- `exec::sort_window::tests::merge_sinks_combines_parallel_partitions`.
- `exec::sort_window::tests::window_functions_row_number_rank_lag_lead_first_last_and_frames` — covers peer ties on `Rank`/`DenseRank`/`LastValue` default frame, `Lag`/`Lead` positive/0/negative offsets, `ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING`, and 0-row input.

### Done when

- `cargo fmt --all && cargo test -p cetra-query && cargo clippy -p cetra-query --all-targets -- -D warnings && cargo doc -p cetra-query --no-deps && cargo fmt --all -- --check` passes.

**Commit:** `query: add spillable external Sort and Window operators`

---

## QE3.4 — Spillable `HashAgg`, `DDSketch`, and `sessions` aggregates (`exec/hash_agg.rs`) *(parallel with QE3.2, QE3.3, QE3.5, QE3.6)*

**Goal:** Implement partitioned spillable `HashAgg` with all Shape B SQL and structured-read aggregates, including bounded-memory `DDSketch` (`approx_percentile_cont`, ≤ 1% relative error, D-QE-13), exact `percentile_cont`, exact `Decimal128(38, 18)` cost sums (rejecting non-finite `F64`, D-A2-4), `COUNT(DISTINCT)` (spillable), `ARRAY_AGG`, `FILTER (WHERE ...)`, and dedicated `sessions` aggregates (`FirstNonNullByOrder`, `TagUnion`).

**Edit:**
- `crates/query/src/exec/hash_agg.rs` (new).
- `crates/query/src/exec/mod.rs`: add `pub mod hash_agg;`.

### Contracts

`crates/query/src/exec/hash_agg.rs`:
- **`DdSketch` (`pub struct DdSketch`):**
  - Relative accuracy `alpha = 0.005` (≤ 0.5%, within D-QE-13's ≤ 1% tolerance), bounded bucket count (`max_buckets = 2048` collapsing lowest buckets so per-group RAM ≤ ~16 KiB), handles positive, zero, and negative finite `f64`s (ignores `NaN`/nulls), exposes `new()`, `add(f64)`, `merge(&DdSketch)`, `quantile(p: f64) -> Result<Option<f64>, QueryError>` (rejecting invalid `p` with `QueryError::Failed`), `count() -> u64`.
- **`AggFunc` & exact output `DataType`:**
  - `CountStar` → `I64` (0 on empty), `Count(Expr)` → `I64` (0 on empty), `CountDistinct(Expr)` → `I64` (0 on empty)
  - `Sum(Expr)` → `I64` (checked overflow → `Failed`), `F64`, or `Decimal128(38, s)` matching input (`NULL` on empty)
  - `SumCostDecimal(Expr)` → `Decimal128 { precision: 38, scale: 18 }` (rejects `NaN`/`±inf`/out-of-range `F64` with `QueryError::Failed`, sums exact `1e-18` USD `i128` units, `NULL` if all input rows null — caller/views cast to `F64` if desired, D-A2-4)
  - `Avg(Expr)` → `F64` (or `Decimal128` on `Decimal128` input; `NULL` on empty)
  - `Min(Expr)`, `Max(Expr)` → same type as input (`f64_cmp` for `F64`; `NULL` on empty)
  - `BoolOr(Expr)`, `BoolAnd(Expr)` → `Bool` (`NULL` on empty)
  - `ApproxPercentileCont { expr: Expr, quantile: f64 }` → `F64` (`DdSketch`; `NULL` on empty)
  - `PercentileCont { expr: Expr, quantile: f64 }` → `F64` (exact sorted `Vec<f64>` with linear interpolation, unspillable `Reservation`; `NULL` on empty)
  - `ArrayAgg { expr: Expr, distinct: bool }` → `List(input_type)` (unspillable `Reservation`; `NULL` on empty)
  - `FirstNonNullByOrder { value: Expr, order_by: Vec<SortKey> }` → same type as `value` (dedicated internal aggregate for `sessions.user_id` ordered by `(start_time ASC, trace_id ASC)` ignoring nulls)
  - `TagUnion(Expr)` → `List(Utf8)` (sorted distinct union of `List(Utf8)`; returns non-null empty `[]` when all inputs are empty, matching `views/sessions.rs:55`!)
- **`AggExpr { pub func: AggFunc, pub filter: Option<Expr>, pub output_name: String }`**
- **`HashAggSpec { pub group_by: Vec<(String, Expr)>, pub aggs: Vec<AggExpr> }`**
- **Concrete execution API & partial-state spill/combine codec:**
  - `HashAgg::new(spec: HashAggSpec, input_schema: Arc<Schema>, ctx: ExecContext) -> Result<Self, QueryError>`
  - `pub fn output_schema(&self) -> Arc<Schema>`
  - **Partial-state `Batch` representation (used for BOTH `SpillWriter` spilling AND parallel multi-worker combine!):** each accumulator encodes its partial state into `cetra_column::Column`s (`Count`/`Sum`/`SumCostDecimal`/`Min`/`Max`/`BoolOr`/`BoolAnd` → 1 scalar column; `Avg` → `(sum, count)` 2 columns; `CountDistinct`/`ArrayAgg`/`TagUnion`/`PercentileCont` → `List(T)`; `DdSketch` → `Binary` compact serialized buckets; `FirstNonNullByOrder` → `(value, order_keys...)`) with a `merge_state` method.
  - Implements `PushSink`: `consume` folds incoming batches into the in-memory `HashMap<GroupKey, AccStates>`; on **any** `ColumnError::MemoryLimit { .. }` during grouped aggregation with `buffered_rows > 0`, it drains the entire in-memory `HashMap` into 32 hash-partitioned partial-state `SpillFile`s (releasing 100% of `HashMap` RAM), and routes subsequent batches through the same 32-partition partial-state spill path!
  - `pub fn into_batches(self) -> Result<Vec<Batch>, QueryError>` (for global agg, emits 1 row even on 0 input rows; for spilled grouped agg, merges one partition at a time via `merge_state`, recursively re-partitioning with salted hash up to depth 4 → `QueryError::TooLarge`).
  - `pub fn execute_hash_agg(source: &dyn ScanSource, scan_spec: &ScanSpec, agg_spec: HashAggSpec, ctx: &ExecContext) -> Result<Vec<Batch>, QueryError>` — runs parallel source partitions through per-worker `HashAgg` sinks and combines their partial states cleanly (supporting `CountDistinct`, `Avg`, `DdSketch`, `PercentileCont`, `ArrayAgg`, `FirstNonNullByOrder`, `TagUnion`, and spilled sinks seamlessly!).

### Tests

- `exec::hash_agg::tests::ddsketch_accuracy_and_merge_within_one_percent`.
- `exec::hash_agg::tests::all_aggregates_filter_where_decimal_cost_and_sessions` — verifies `SumCostDecimal` returns `Decimal128(38,18)` and rejects `NaN`, `TagUnion` returns empty `[]` on all-empty group, `FirstNonNullByOrder`, `FILTER (WHERE ...)`, empty-input global agg = 1 row.
- `exec::hash_agg::tests::hash_agg_spills_under_tight_memory_with_count_distinct`.

### Done when

- `cargo fmt --all && cargo test -p cetra-query && cargo clippy -p cetra-query --all-targets -- -D warnings && cargo doc -p cetra-query --no-deps && cargo fmt --all -- --check` passes.

**Commit:** `query: add spillable HashAgg, DDSketch, and sessions aggregates`

---

## QE3.5 — Spillable `HashJoin`, `SemiJoin`, `AntiJoin`, and `CrossJoin` (`exec/hash_join.rs`) *(parallel with QE3.2–QE3.4, QE3.6)*

**Goal:** Implement spillable Grace `HashJoin` supporting `Inner`, `Left`, `Right`, `Full`, `LeftSemi`, `LeftAnti`, and bounded `Cross` joins, with multi-column equi-keys, configurable `null_equals_null`, and optional residual non-equi filter predicates.

**Edit:**
- `crates/query/src/exec/hash_join.rs` (new).
- `crates/query/src/exec/mod.rs`: add `pub mod hash_join;`.

### Contracts

`crates/query/src/exec/hash_join.rs`:
- `pub enum JoinType { Inner, Left, Right, Full, LeftSemi, LeftAnti, Cross }`
- `pub struct HashJoinSpec`:
  - `pub join_type: JoinType`
  - `pub left_keys: Vec<Expr>`, `pub right_keys: Vec<Expr>` (empty only for `Cross`)
  - `pub null_equals_null: bool` (`false` for SQL `ON a = b`; `true` for `INTERSECT`/`EXCEPT` lowering per D-QE-14)
  - `pub residual_filter: Option<Expr>` (bound against combined `(left_cols..., right_cols...)` schema)
- **Concrete execution API:**
  - `pub fn join_output_schema(join_type: JoinType, left: &Schema, right: &Schema) -> Arc<Schema>` (`LeftSemi`/`LeftAnti` output `left` schema only; `Inner`/`Left`/`Right`/`Full`/`Cross` concatenate `left` + `right`, widening non-matching sides to nullable on `Left`/`Right`/`Full`).
  - `pub fn execute_hash_join(left: Vec<Box<dyn PartitionStream>>, left_schema: Arc<Schema>, right: Vec<Box<dyn PartitionStream>>, right_schema: Arc<Schema>, spec: HashJoinSpec, ctx: &ExecContext) -> Result<Box<dyn PartitionStream>, QueryError>`
- **Exact semantics & Grace spill rules:**
  - Builds hash table on the right side (for `LeftSemi`/`LeftAnti`, always builds on right so each left row emits at most once even when multiple right rows match!).
  - When `residual_filter` is `Some`, a pair matches ONLY when both equi-keys AND `residual_filter` evaluate to `true` (`Right`/`Full` mark a build row matched only after `residual_filter` passes; `Left`/`Full` emit `(left, NULLs)` if all key-matching build rows fail `residual_filter`).
  - **Grace spill:** on **any** `ColumnError::MemoryLimit { .. }` while building with `buffered_rows > 0`, partition both build and probe streams into `NUM_JOIN_PARTITIONS = 32` `SpillFile`s by key hash (rows with NULL join keys when `!null_equals_null` bypass spill directly — emitting unmatched probe rows for `Left`/`Full`/`LeftAnti` and unmatched build rows for `Right`/`Full`!). Recursive salted re-partitioning up to depth 4 → `QueryError::TooLarge` on severe key skew!
  - `CrossJoin`: buffers right side under unspillable `Reservation`, checks `CancelToken` every output batch, returns `QueryError::TooLarge` on `MemoryLimit`.

### Tests

- `exec::hash_join::tests::all_join_types_null_keys_and_residual_filter` — verifies `Inner`, `Left`, `Right`, `Full`, `LeftSemi`, `LeftAnti`, `Cross`, `null_equals_null = false` vs `true`, and `residual_filter` edge cases (`Left`/`Right`/`Full`/`LeftSemi`/`LeftAnti`).
- `exec::hash_join::tests::grace_hash_join_spills_under_tight_memory` — forces Grace spill across `Inner`, `Left`, `Right`, `Full`, `LeftSemi`, `LeftAnti` including NULL-key rows under a tight `ScopedPool`.

### Done when

- `cargo fmt --all && cargo test -p cetra-query && cargo clippy -p cetra-query --all-targets -- -D warnings && cargo doc -p cetra-query --no-deps && cargo fmt --all -- --check` passes.

**Commit:** `query: add spillable Grace HashJoin, SemiJoin, AntiJoin, and CrossJoin`

---

## QE3.6 — Storage `AsyncAdmissionPool`, `Disk` spill cap, `MetadataCache`, zero-Arrow `ParquetSource`, `MemtableSource`, and `Suspects::report_corrupt` *(parallel with QE3.2–QE3.5)*

**Goal:** Build `crates/storage/src/engine/` with async query admission (`tokio::sync::Notify` around `cetra_column::GlobalPool`), frozen `Disk` spill cap, bounded LRU Parquet `MetadataCache`, zero-Arrow low-level `ParquetSource` (using `ParquetMetaDataPushDecoder` + `Sbbf::from_bytes` + `SerializedPageReader` + `unwind::guard_sync` + `Suspects::report_corrupt`), and `MemtableSource`.

**Edit:**
- `crates/storage/Cargo.toml`: move `cetra-query.workspace = true` from `[dev-dependencies]` to `[dependencies]`.
- `Cargo.lock`: refresh via `cargo metadata --format-version 1 > /dev/null`.
- `crates/storage/src/disk.rs`: add `engine_spill_cap: AtomicU64` + `engine_spilled: OnceLock<Arc<AtomicU64>>` (interior-mutable so `Arc<Disk>` accepts registration via `pub(crate) fn set_engine_spilled(&self, counter: Arc<AtomicU64>)`, which immediately refreshes `self.apply_spill_cap(self.free())`), updated inside `apply_spill_cap(free)` at `statvfs` measurement time (`spilled_now + free.saturating_sub(self.min_free)`), plus `pub(crate) fn engine_spill_cap(&self) -> u64`.
- `crates/storage/src/verify.rs`: add `pub(crate) fn report_corrupt(&self, id: FileId)` queueing `id` into `self.inner.queue.suspect([id], catalog::now_ns())` and emitting the same privacy-safe `tracing::warn!`.
- `crates/storage/src/lib.rs`: add `pub(crate) mod engine;`.
- `crates/storage/src/engine/{mod.rs,admission.rs,metadata_cache.rs,parquet_source.rs,mem_source.rs}` (new).

### Contracts

1. **`crates/storage/src/engine/admission.rs` (`AsyncAdmissionPool`):**
   - Wraps `Arc<cetra_column::GlobalPool>` + `Arc<tokio::sync::Notify>` (passing `move || notify.notify_waiters()` to `GlobalPool::with_release_hook`).
   - `pub async fn admit(&self, initial: usize, cap: usize, wait: Duration) -> Result<Arc<ScopedPool>, cetra_query::QueryError>`: exact race-free `notified.enable()`-before-`try_admit` loop with `tokio::time::timeout_at`, returning `Err(cetra_query::QueryError::Busy)` on deadline expiry.
2. **`crates/storage/src/engine/metadata_cache.rs` (`MetadataCache`):**
   - Bounded LRU cache keyed by `FileId`, storing `Arc<parquet::file::metadata::ParquetMetaData>`, weighted by `meta.memory_size()`, capped at `max_bytes` (`0` disables caching).
3. **`crates/storage/src/engine/parquet_source.rs` (`ParquetSource` — zero `parquet/arrow`):**
   - Takes `store: Arc<dyn ObjectStore>` (from `objects.store()` in production, `InMemory` in unit tests), `table: FileTable`, `schema: Arc<Schema>`, `files: Vec<Arc<FileMeta>>`, `deletions: Arc<[Arc<PendingDeletion>]>` (gated per file via `crate::deletion::applicable(&deletions, Some(file.deletions_applied_seq), if table == FileTable::Traces { i64::MIN } else { file.min_ingest_time })` and mapped to `Vec<TombstoneSpec>` for `cetra_query::tombstone::covered_predicate`), `metadata_cache: Arc<MetadataCache>`, `suspects: Suspects` (already `Clone` over `Arc`), `runtime: tokio::runtime::Handle`.
   - **Cancellation-aware I/O bridge:** Inside `PartitionStream::next_batch` (always run on `WorkerPool`, never directly on Tokio worker threads), `fetch_ranges(store, path, ranges, cancel)` races `store.get_ranges(...)` against `cancel.is_cancelled()` (polling every `1ms` inside `tokio::select!` alongside `store.get_ranges(...)`) so cancelling a query aborts stalled S3/network I/O in `<= 1ms` and returns `QueryError::Cancelled`!
   - **Metadata loading & corruption safety:**
     - Checks `MetadataCache` first; on miss, checks `file.size >= 8`, clamps speculative tail fetch to `tail_len = file.size.min(file.footer_size.max(8))`, fetches `[file.size - tail_len .. file.size]`, validates the last 8 bytes with `crate::parquet::footer_size_from_tail` (verifying magic `"PAR1"` and `footer_len + 8 <= file.size` **before** calling `ParquetMetaDataPushDecoder` to block the 8-byte fuzz regression underflow!), then loops `ParquetMetaDataPushDecoder::try_new(file.size)?.with_page_index_policy(PageIndexPolicy::Optional)` up to 4 `NeedsData` rounds validating every requested range `r.start <= r.end && r.end <= file.size`, all wrapped inside `unwind::guard_sync`.
     - Any non-store `ParquetError`, `object_store::Error::NotFound`, or decoder panic calls `suspects.report_corrupt(file.id)` and returns `QueryError::internal("corrupt parquet file")`. Transient object-store errors (timeouts, 503, I/O) return `QueryError::internal("storage read error")` **without** quarantining the file.
   - **Pruning:**
     - File-level: `time_range` (±1 day anchor slack for `Spans`/`Traces`, exact for `Scores`; `around_ns` ±1 day ONLY for `Spans`/`Traces` when `spec.id_lookups` contains a **`trace_id`** lookup (`IdLookupHint::Fixed16 { column: "trace_id", .. }`) and `time_range` is open — **NEVER** for `user_id`/`session_id`/`span_id` or `Scores`, per D154 & `query/mod.rs:405,513`!) + `[file.min_time, file.max_time]`.
     - Row-group level: min/max `Statistics` against pushed predicates + `time_range` + `id_lookups`; `Sbbf::from_bytes` bloom-filter checks on `trace_id`, `session_id`, `user_id`, `score_id`.
     - Page level: `ColumnIndex`/`OffsetIndex` row-range selection. **IMPORTANT (`parquet 59.3.2`):** Always pass the **full** `page_locations` slice to `SerializedPageReader::new_with_properties` (never a pruned subset, which corrupts dictionary offset math in `serialized_reader.rs:642-661`), fetch only the optional dictionary page + kept data pages into `SliceChunkReader`, consume the dictionary page via `get_next_page()` **before** calling `skip_next_page()` on pruned data pages (`serialized_reader.rs:1143-1151` pops an unread dictionary page on `skip_next_page()`!), and skip pruned data pages cleanly!
   - **Zero-Arrow low-level decoding + `DictColumn` preservation + late materialization:**
     - Decodes every stored Parquet column type directly from `SerializedPageReader` / `parquet::column::reader::get_column_reader`:
       - Dictionary-encoded `Utf8` columns: read `Page::DictionaryPage` + RLE/hybrid data page indices directly from `SerializedPageReader` into `cetra_column::DictColumn` (falling back to `Flat` if a page uses `PLAIN` encoding).
       - Scalar columns: `Bool`, `Int32`/`Int64`/`UInt16` (`spans.schema_version` is physical `INT32` → `v as u16 as i64`) → `I64`, `Double` → `F64`, `Timestamp(Ns, UTC)` → `TimestampNs { utc: true }`, `ByteArray` (`Utf8`/`Binary`), `FixedLenByteArray(8|16)` → `FixedBytes8`/`FixedBytes16`.
       - Multi-leaf nested columns: map top-level field to its `1..=3` leaf `ColumnDescriptor`s by `path().parts()[0]`, reading each leaf's actual `max_def_level`/`max_rep_level` and keeping sibling leaves lockstepped on `rep == 0`: `List(Utf8)` (incl. dictionary-encoded `spans.conventions`), `Map<Utf8, Utf8?>` (2 leaves), `Events` (3 leaves), `Links` (3 leaves).
     - **Schema evolution:** missing nullable column in an older file produces `Column::Const { value: ScalarValue::Null(field.data_type), len }`.
     - **Late materialization:** decode filter + tombstone key columns first, evaluate `spec.filters` + inline tombstone predicate (`cetra_query::tombstone::covered_predicate`) into `Selection::Mask`, skip fetching/decoding heavy payload columns (`input`, `output`, `attributes`, `resource_attributes`, `events`, `links`, `input_preview`, `output_preview`, `metadata`) for 0-hit row groups, and project to `projected_schema(spec)` (including empty `projection = Some(vec![])`!).
4. **`crates/storage/src/engine/mem_source.rs` (`MemtableSource`):**
   - Filters `MemSnapshot::partitions` by `project: ProjectId`, optional `keep_from: Option<NaiveDate>`, exact `day_overlaps`, and `[batch.min_time, batch.max_time]`.
   - Converts each surviving Arrow `RecordBatch` via **`crate::column_bridge::record_batch_to_column`**, explicitly reserving `batch.memory_bytes()` in `res` (rolling back on error), evaluates applicable tombstones (`deletion::applicable(deletions, None, batch.min_ingest_time)`) and `spec.filters` exactly, and projects to `projected_schema(spec)`.

### Tests

- `engine::admission::tests::admit_waits_for_release_and_times_out_with_busy`.
- `engine::metadata_cache::tests::lru_eviction_and_zero_disables_cache`.
- `engine::parquet_source::tests::reads_all_stored_types_prunes_bloom_and_schema_evolves` — runs on `WorkerPool` against real written `spans`, `traces`, `scores` Parquet files (covering `DictColumn`, `UInt16`, `List`, `Map`, `Events`, `Links`, bloom pruning, page pruning, empty projection `Some(vec![])`, missing nullable column → `Const(Null)`).
- `engine::parquet_source::tests::corrupt_eight_byte_file_reports_suspect_without_panicking` — replays the exact 8-byte fuzz regression (`2f 15 00 00 "PAR1"`) and verifies `Err(QueryError::Internal(_))` + `VerifyQueue` contains `file.id`.

### Done when

- `cargo fmt --all && cargo test -p cetra-storage --lib engine:: && cargo clippy -p cetra-storage --lib --tests -- -D warnings && cargo fmt --all -- --check` passes.

**Commit:** `storage: add async admission pool, Parquet metadata cache, zero-Arrow ParquetSource, and MemtableSource`

---

## QE3.7 — Native `TracesSource`, `ScoresSource`, and M3 differential & fault tests

**Goal:** Build native spillable `NativeTraceMerger` / `TracesSource` (exact 30-column rollup + widening-window 5-slice list mode + generalized two-phase rank→fetch for non-time sorts) and native `ScoresSource` (`(updated_at DESC, ingest_time DESC)` LWW dedup) in `crates/storage/src/engine/`, and verify `ParquetSource`, `TracesSource`, and `ScoresSource` differentially against DataFusion.

**Edit:**
- `crates/storage/src/engine/{mod.rs,traces_source.rs,scores_source.rs,diff_tests.rs}`.

### Contracts

1. **`crates/storage/src/engine/traces_source.rs` (`NativeTraceMerger` & `TracesSource`):**
   - Exact 30-column rollup matching `crates/storage/src/partials.rs`:
     - `Rank { root: u8, start_time: i64, tie: Option<Vec<u8>> }` (`tie = root_span_id`, `None` on non-root partials), tie-breaking lexicographically by `(rank, value)`;
     - `RootSpanId` / `Status` only update when `has_root == true`; `status_of` (`error_count > 0` → `"error"`, else ranked root `status`, else `"unset"`); `preview = true` on all partials;
     - `cost_total`: saturating `i128` sum in `1e-18` USD units (`partials.rs:568-575`), preserving `NULL` when all partials are null;
     - `Tags`/`Models`/`Prompts`/`Metadata` emit non-null `[]`/`{}` when empty; `HasRoot` defaults to `false`; `partial_count` is `I64` in `cetra-column` (widened from Parquet `Int32`);
     - `MergeColumns` minimal input projection (`requested ∪ {trace_id, start_time, has_root, root_span_id}` + `{error_count}` if `status` is requested).
   - **Mandatory pushdown safety invariant:** ONLY `start_time ∈ [a - trace_lookback, b + trace_lookback]` (saturating) and `trace_id =`/`IN` are pushed to partial scans (`ParquetSource` + pre-filtered `spans_mem` cached partials — `spans_mem` passed into `TracesSource` MUST already be filtered by `project_snapshot` with `project`, `keep_from`, and span-level `user_id`/`ingest_time` tombstones!). All user filters and the unpadded `start_time ∈ [a, b]` check evaluate **after** merging.
   - **`traces` tombstone grouping:** matches `query/traces.rs:298-358` — checks `seq > file.deletions_applied_seq` only (`i64::MIN` cutoff), grouping gated files by their exact pending `Vec<u64>` seqs.
   - **Streaming sorted merge (spillable):** feeds partial batches into QE3.3's `ExternalSort` by `trace_id` (`merge_sinks`), streaming chunks through `NativeTraceMerger` (splitting at the last `trace_id` run boundary so merger RAM stays ~2 MiB/partition!).
   - **Three execution modes & `explain_summary` markers (`mode=lookup`, `mode=list`, `mode=range`):**
     - `mode=lookup`: direct `trace_id` lookup with optional `around_ns` ±1 day hint;
     - `mode=list` (widening-window 5-slice list mode `15m / 1h / 6h / 24h / whole-range`, matching `query/traces.rs:1149-1409`): resolves open bounds via `data_bounds` from files + memtable; Phase 1 ranks narrow columns per newest-first slice by `(start_time DESC, trace_id DESC)` via `TopK(fetch)`, deduplicating across slices via `seen: BTreeSet<[u8; 16]>` and recording each winner's `slice` index; Phase 2 fetches full requested columns per winning `slice` under that slice's exact padded bounds!
     - `TraceListMode::TwoPhase { sort_keys, limit, offset }`: for non-time sorts (`cost_total`, `end_time - start_time` for `duration_ns`, `total_tokens`), extracts referenced column names from `sort_keys[i].expr` (e.g. `end_time` and `start_time` for `duration_ns`, since `duration_ns` is computed, not stored!), remaps `sort_keys` onto Phase 1's narrower merged schema, ranks via `TopK(offset + limit)`, slices `offset` **before** Phase 2 so Phase 2 fetches only `<= limit` winning `trace_id`s, and reorders Phase 2's output to match Phase 1's winning order!
2. **`crates/storage/src/engine/scores_source.rs` (`ScoresSource`):**
   - **Mandatory pushdown safety invariant:** ONLY predicates whose column references are non-empty and all equal `"created_at"` are pushed below LWW dedup (exact day pruning, no ±1 day slack, no `around_ns`!).
   - Extends underlying scan projection to `requested ∪ filter_cols ∪ {score_id, updated_at, ingest_time, deleted}`, applies GDPR pending deletions before LWW, deduplicates per `score_id` via `ExternalSort` on **`(score_id ASC, updated_at DESC NULLS FIRST, ingest_time DESC NULLS FIRST)`** (matching `query/scores.rs:133-134`) + streaming first-of-group filter, drops `deleted == true`, applies post-dedup `spec.filters` and optional `limit`, and re-projects to `self.projected_schema(spec)`.
3. **`crates/storage/src/engine/diff_tests.rs`:**
   - **ParquetSource parity** against DataFusion across multi-day `spans`, `traces`, `scores` files with tombstones, bloom lookups, and late materialization.
   - **TracesSource parity** (range, lookup, widening-window list, AND two-phase `cost_total`/`duration_ns`/`total_tokens` sort) row-for-row against DataFusion `TracesProvider` on multi-file + memtable split traces, root/non-root tie breaks, deleted traces, and lookback boundary spans.
   - **ScoresSource parity** row-for-row against DataFusion `ScoresProvider` with multiple updates per `score_id`, `updated_at` ties broken by `ingest_time`, logical `deleted = true` resurrection prevention, and GDPR tombstones.
   - **Spill + cancel + admission integration tests:** tight `ScopedPool` triggering spills inside `TracesSource` and `ScoresSource`, plus sub-millisecond `CancelToken` abort verifying synchronous worker drain and zero leaked reservations.

### Done when

- `cargo fmt --all && cargo test -p cetra-storage --lib engine:: && cargo clippy -p cetra-storage --lib --tests -- -D warnings && cargo fmt --all -- --check` passes.

**Commit:** `storage: add native TracesSource, ScoresSource, and M3 differential tests`

---

## QE3.8 — Neon code review, fixes, and M3 closeout

**Goal:** Run a thorough Neon code review over all M3 code (`crates/query/src/exec/` and `crates/storage/src/engine/`), fix all findings, update `docs/roadmap.md`, and pass full `just check`.

**Edit:**
- Review fixes across `crates/query/src/exec/` and `crates/storage/src/engine/`.
- `docs/roadmap.md`: update track `QE` status to M1–M3 done.

### Done when

- `just check` passes.
- `just check-workflows` passes.
- `just assert-clean` passes.
- `cargo tree -p cetra-query` confirms zero `tokio`, `arrow`, `parquet`, `datafusion`, `object_store`.

**Commit:** `query: close out track QE M3`
