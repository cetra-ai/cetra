# Track QE — Milestone 6 Implementation Plan (Storage Internals, WAL Framing & Memtable Migration)

**Status:** Approved (Neon R2: 0 Blockers; Major 1 & Minors resolved)
**Track / Milestone:** QE — M6 (`docs/plans/query-engine/design.md` § Milestones / M6)
**Depends on:** M1 (`cetra-column` + binary framing), M2 (`cetra-query::tombstone`), M3 (including QE3.6 fixes + QE3.7 `TracesSource`/`ScoresSource`), and **M4 QE4.1** (which lands `Storage.qe_runtime` in `lib.rs` once; after QE4.1 merges, QE6.1–QE6.6 run in parallel alongside QE4.2–QE4.5 and M5 with zero `lib.rs` conflict!).
**Leaves out:** breaking public `cetra-core`/`cetra-storage` API signatures, migrating `cetra-otel`/`cetra-server`/`cetra-cli`/`web/`, deleting DataFusion from `Cargo.toml` and `crates/storage/src/{query,views,reads,sql}/` (all atomic in M7).

---

## Goal

Migrate every remaining internal subsystem of `cetra-storage` off DataFusion and Arrow onto `cetra-column` and `cetra-query` — compaction k-way merge and slicing, GDPR deletion candidate checking (`Limit(1)`) and rewriting, media GC live-set scanning, `anchor`, `dedup`, `partials`, `flush`, `verify`, `wal` binary batch framing (+ LZ4, CRC-32C, `FORMAT_VERSION = 1`), `memtable`, `golden_tests/dump`, and `fuzz_entry` — while isolating ALL storage Parquet reading & writing inside one internal `file_format` adapter module and keeping all public `cetra-storage` APIs bridged so `just check` stays 100% green until M7's atomic cut-over.

---

## Cross-Cutting Invariants (Apply to Every Phase)

1. **Prerequisite gate:** M6 starts ONLY after M3 (including QE3.6 fixes and QE3.7 `TracesSource`/`ScoresSource`) is merged and green on `qe-query-engine`.
2. **Unchanged public `cetra-storage` signatures until M7:**
   - `Storage::append_spans(ProjectId, RecordBatch)` and `Storage::append_scores(ProjectId, RecordBatch)` convert at the crate boundary (`validate_batch` -> `column_bridge::record_batch_to_column` -> `batch.memory_bytes()` -> `writer.append`).
   - `crates/storage/src/error.rs` keeps its 5 public Arrow/Parquet-embedding enums (`WalError`, `MemtableError`, `AnchorError`, `PartialsError`, `ParquetWriteError`) plus `MediaError::Query(DataFusionError)` and `CompactionError::Read { source: DataFusionError }` intact until M7, adding `pub(crate)` helper constructors (`from_column`, `AnchorError::is_store_error`, deterministic `QueryError -> DataFusionError` bridge) so migrated modules drop all `arrow`/`parquet`/`datafusion` imports cleanly.
   - **Deterministic `may_be_corrupt` bridge (`quarantine_tests.rs:294`, `unwind.rs:516`):**
     - `QueryError::TooLarge` -> `DataFusionError::ResourcesExhausted(...)` (`read_source() == Some("resources_exhausted")`, `may_be_corrupt == false`).
     - Corrupt Parquet error (`QueryError::Internal("corrupt parquet file")` or dedicated corrupt variant) -> `DataFusionError::ParquetError(Box::new(ParquetError::General(...)))` (`may_be_corrupt == true`, ensuring `a_failing_merge_suspects_its_inputs` queues ALL merge input files!).
     - Transient store error (`QueryError::Internal("storage read error")`) -> `DataFusionError::External(...)` (`may_be_corrupt == false`).
   - **Legacy DataFusion oracle bridge (only 2 call sites!):** `query/mod.rs::rows_for` returns `Vec<Arc<cetra_column::Batch>>` and converts inside `query/mod.rs::scan_union` via `column_bridge::column_to_record_batch(&batch, Some(scan.schema.as_ref()))` (zero edits to `query/{spans,scores}.rs`!), and `query/traces.rs` passes `Some(traces_schema().as_ref())` (**MUST pass `Some(schema)`**, never `None`, so `UInt16` `schema_version`, `Int32` `partial_count`, and non-nullable `utf8_list()` items match DataFusion's table schema!).
3. **WAL framing, `FORMAT_VERSION = 1` & recursive anti-bomb guard (D-QE-7):**
   - Keep `FORMAT_VERSION = 1` and keep the outer 17-byte WAL record header (`payload_len: u32`, `crc32c: u32`, `seq: u64`, `kind: u8 = 1`, CRC-32C over `seq || kind || payload`) unchanged so `wal/replay.rs` torn-tail truncation and segment recovery are preserved verbatim.
   - Replace Arrow IPC payload body with `cetra_column::frame::{encode_batch_with_limit, decode_batch}`, deleting all ~200 lines of Arrow IPC FlatBuffer guards.
   - **Recursive anti-bomb guard in `wal/record.rs::read_batch`:** Reject `payload.len() > MAX_PAYLOAD` (256 MiB) AND **recursively check `is_all_flat(col)`** (including recursing into `FlatData::List { child, .. }`) to reject any `Const`/`Rle`/`Dict` at either top level OR nested list child level (blocking both `Column::Const(len = u32::MAX)` and `FlatData::List { offsets: [0, u32::MAX], child: Column::Const(u32::MAX) }` DoS bombs!).
   - Regenerate `fuzz/corpus/wal_record/*` and `fuzz/corpus/wal_replay/*` via `just fuzz-seed` (existing Arrow IPC files in `fuzz/regressions/wal_*` remain safe and cleanly return `InvalidPayload` without panicking).
4. **Never call `ParquetPartitionStream::next_batch` on a Tokio worker thread:**
   - `merge_slice`, `holds_covered`, and `MediaGc::live_set` MUST execute ALL `ParquetSource`/`UnionSource` stream pulls off Tokio worker threads (via `WorkerPool` or `tokio::task::spawn_blocking`), never directly inside an `async fn` on a Tokio worker thread.
5. **All 4 `Column` encodings (`Flat`, `Const`, `Dict`, `Rle`) & `Selection`:**
   - `ParquetSource` emits `Dict` and `Const` columns. `dedup`, `partials`, `anchor`, `memtable`, `flush`, and `lib.rs::project_of` MUST handle all 4 `Column` encodings and non-`Selection::All` batches cleanly.
   - Slice/reorder batches via `batch.with_selection(Selection::Indices(indices))?.compact(pool)` (note `cetra-column::Batch` does NOT have `Batch::take`, and `Selection::Indices` replaces `self.selection` using **physical** row indices `0..raw_len`, so callers on potentially filtered batches MUST collect physical indices via `batch.selection().iter_active(batch.raw_len())` or `compact` first!).
6. **Compaction memory, slice & ordering invariants:**
   - Compaction uses its own dedicated `GlobalPool(compaction.max_memory)` (separate from query pool, NO real spill dir: pass `SpillManager::with_fixed_cap(PathBuf::new(), 0)` to `ExecContext` since compaction never spills).
   - Preserve dynamic `batch_rows` clamping (`MIN_BATCH_ROWS = 64` .. `ctx.batch_size`) AND hold `ScopedPool` `Reservation`s for each stream's current `Batch`, the `pending` group tail, and the chunk being built (releasing as chunks emit) so `compaction.max_memory` is strictly enforced (`TooLarge -> ResourcesExhausted`).
   - **Exclusive `Slice::upper` vs inclusive `ScanSpec::time_range.max_ns`:** `Slice` is `[lower, upper)` (exclusive), whereas `ScanSpec::time_range` is `[min_ns, max_ns]` (inclusive). Set `max_ns: slice.upper.and_then(|u| u.checked_sub(1))` (short-circuiting empty if `slice.upper == Some(i64::MIN)`) AND include exact `time < upper` in `ScanSpec::filters`.
   - K-way merge over per-file `ParquetSource` streams ordered by `file.id` (UUIDv7) ascending preserves exact sort order (`Spans`: `(trace_id ASC, start_time ASC)`; `Scores`: `(created_at ASC)`) and breaks equal-key ties by `(input_index ASC, row_index ASC)`.
   - Split chunks strictly on `same_group` boundary (`spans.trace_id`; `scores.created_at`).
7. **GDPR deletion `holds_covered` soundness invariants (CRITICAL — zero data leaks!):**
   - Pass **`deletions: Arc::from([])`** to `ParquetSource::new` (inline tombstones MUST be disabled so `ParquetSource` does not strip the covered rows before `spec.filters` sees them!).
   - Leave **`ScanSpec.time_range` open** (since `time_range` prunes `start_time`/`created_at`, NOT `ingest_time`!) and instead add conjunctive `ingest_time <= max_cutoff` to `spec.filters` (which `extract_expr_constraints` uses for row-group/page min-max pruning on `ingest_time`).
   - Populate **`ScanSpec.id_lookups` ONLY when EVERY applicable deletion targets the exact same single column** (e.g., `Traces`-only deletions -> single `Fixed16 { column: "trace_id", .. }`), and **union all their values into ONE single `IdLookupHint`** (pushing multiple `IdLookupHint`s on the same column ANDs them together in `ScanPlan::compile`, and `User`/multi-target deletions are disjunctive `OR`!).
   - Stop the caller pull loop immediately on the first non-empty batch (`batch.num_rows() > 0`) (note `ParquetSource` does not enforce `spec.limit` internally).
   - Error logs in `compaction/{job,rewrite}.rs` log `error.label()` / `error.read_source()` only.
8. **Single file-format adapter isolation & hygiene (`AGENTS.md`):**
   - After M6, zero production storage modules outside `file_format/` (plus `error.rs` / `verify.rs`'s legacy DF error-classifier helpers `may_be_corrupt`/`is_store_error`/`Suspects::report`, and temporary M1–M6 oracle modules `query/`/`views/`/`reads/`/`sql/`/`column_bridge.rs` deleted in M7) import `parquet::*` or `arrow::*` for reading/writing data.
   - `#![forbid(unsafe_code)]`, zero `unwrap()`/`expect()` outside tests, no `#![allow(...)]` (remove stale `#![allow(dead_code)]` directives in touched modules).

---

## Phased Breakdown & Parallelism DAG

```text
QE6.1 Foundation:
      - `file_format/` adapter (BatchFileWriter + write_file_batch + zero-Arrow
        verify_parquet_file / read_time_column / read_trace_anchors)
      - `FileTable::column_schema` (+ `wal::Table::column_schema` delegating)
      - `error.rs` `pub(crate)` conversion helpers (`from_column`, `AnchorError::is_store_error`,
        deterministic `QueryError -> DataFusionError` bridge)
      - `dedup.rs` (`latest_spans_batch`, `latest_scores_batch`)
      - `partials.rs` (`partials_from_spans_batch` owning reusable `TraceAcc` + `Usd(i128)` sum)
      - `deletion.rs` (`PendingDeletion::to_tombstone_spec`, `to_tombstone_specs`, `drop_covered_batch`)
      - Opaque `MergeContext::new(...)` and `MediaGc::new(...)` constructors + `QeRuntime` handles
        on `Storage` (`lib.rs`) so QE6.3 and QE6.4 compile cleanly without editing `lib.rs`!
  ├── QE6.2 WAL binary framing + Memtable on Batch + anchor + flush +               ┐
  │         deletion::project_snapshot + lib.rs WAL/append boundary conversion +     │ 3 PARALLEL
  │         engine/{mem_source,traces_source} zero-copy + 2-line DF oracle bridge +  │ PHASES
  │         fuzz_entry WAL seeds (`just fuzz-seed`)                                  │ (100% file-
  ├── QE6.3 Compaction: merge.rs k-way merge under compaction.max_memory +           │  disjoint!)
  │         slice.rs + job.rs + rewrite.rs GDPR-sound `holds_covered`                │
  └── QE6.4 Media GC streaming live-set scan + verify.rs off parquet/arrow +         ┘
            golden_tests/dump.rs Batch JSON helper
        └── QE6.5 Final cleanup (drop compaction DF RuntimeEnv & temporary QE6.1 wrappers,
                  import audit, model/crash/quarantine/resource/golden/fuzz verification)
              └── QE6.6 Neon Code Review, Fixes & M6 Closeout
```

Because QE6.1 establishes opaque `MergeContext` / `MediaGc` constructors (and `QeRuntime` on `Storage`), QE6.2 owns the `lib.rs` WAL/append boundary edits exclusively, while QE6.3 (`compaction/*`) and QE6.4 (`media.rs`, `verify.rs`, `golden_tests/dump.rs`) do NOT touch `lib.rs` at all — making QE6.2, QE6.3, and QE6.4 **100% file-disjoint** and independently compilable & testable!

---

### Phase QE6.1 — Foundation: `file_format` Adapter, `FileTable::column_schema`, `error.rs` Helpers, `dedup`, `partials`, `deletion`, and Opaque Runtime Constructors

**Files:**
- `crates/storage/src/file_format/mod.rs` (new; wraps `parquet.rs` writer + zero-Arrow reader helpers)
- `crates/storage/src/catalog/mod.rs` (add `FileTable::column_schema(self) -> Arc<cetra_column::Schema>` with `OnceLock` cache for `Spans`, `Scores`, `Traces`)
- `crates/storage/src/wal/mod.rs` (`Table::column_schema(self)` delegating to `FileTable::from(self).column_schema()`)
- `crates/storage/src/error.rs` (`pub(crate)` helpers: `from_column`, `AnchorError::is_store_error`, deterministic `query_err_to_df(err)` preserving `may_be_corrupt` and `ResourcesExhausted`)
- `crates/storage/src/dedup.rs` (`latest_spans_batch`, `latest_scores_batch`, handling all 4 `Column` encodings; temporary Arrow wrappers kept until QE6.5)
- `crates/storage/src/partials.rs` (`partials_from_spans_batch` preserving exact `Usd(i128)` `1e-18` cost accumulation and sharing `TraceAcc` rollup logic)
- `crates/storage/src/deletion.rs` (`PendingDeletion::to_tombstone_spec`, `to_tombstone_specs`, `drop_covered_batch`)
- `crates/storage/src/compaction/merge.rs` (add opaque `MergeContext` fields for compaction `GlobalPool`, `WorkerPool`, `MetadataCache`, `ObjectStore`, `VerifyQueue`, `Handle` alongside existing constructor/helper so QE6.3 compiles without editing `lib.rs`)
- `crates/storage/src/media.rs` (add `engine` handles to `MediaGc` via opaque constructor so QE6.4 compiles without editing `lib.rs`)
- `crates/storage/src/lib.rs` (wire `file_format` module + pass `QeRuntime` / compaction `GlobalPool` / `VerifyQueue` into `MergeContext` and `MediaGc` constructors)

**Deliverables:**
1. **`file_format` adapter:**
   - **Writer:** `BatchFileWriter<W: Write + Send>::new(table, level, sink: W, expected_rows: u64)` / `write(&Batch)` / `finish()` and `write_file_batch(table, level, batches: &[Arc<Batch>])` (stable sort via `cetra_query::exec::SortKey` + `with_selection(Selection::Indices(...))?.compact(None)`, min/max timestamps read directly from `cetra-column`, Arrow conversion with `Some(table_schema(table).as_ref())` strictly inside `write` per 8,192-row slice).
   - **Zero-Arrow reader helpers:**
     - `verify_parquet_file(store, file) -> Verdict`: checks `head` size, uses **`PageIndexPolicy::Required`**, decodes every leaf column of every row group via `SerializedPageReader` + `get_column_reader`, checks total rows == `file.rows`, treats `Fault::Truncated` / `is_store_error` as `Verdict::Unavailable`, and preserves exact static reason strings (`"size differs from the catalog"`, `"missing object"`, `"decode failed"`, `"row count differs from the catalog"`).
     - `read_time_column(store, files, column) -> Result<Vec<i64>, CompactionError>`: projects single timestamp column, rejects nulls, returns sorted `Vec<i64>`, preserves store-vs-corrupt distinction (`may_be_corrupt`).
     - `read_trace_anchors(store, file, since) -> Result<Vec<Batch>, AnchorError>`: projects `[trace_id, start_time, end_time, last_ingest_time]` with row-group `last_ingest_time >= since` stats pruning, preserving `AnchorError::Parquet` + `is_store_error` distinction for `s3_fault_tests.rs:133` and `unwind.rs:510`.
2. **`dedup` / `partials` / `deletion` Batch APIs:** all handle `Flat`/`Const`/`Dict`/`Rle` + `Selection`.

**Tests & Done Criteria:**
- Unit tests for `BatchFileWriter` round-trip, `verify_parquet_file` (all 4 reason strings + mid-page corruption + `Fault::Truncated` -> `Unavailable`), `read_time_column` (corrupt vs store error), `read_trace_anchors` (`since` pruning + `is_store_error`), `latest_spans_batch`/`latest_scores_batch`, `partials_from_spans_batch` (exact parity vs Arrow `partials_from_spans`), and `drop_covered_batch`.
- `cargo test -p cetra-storage --lib && cargo clippy -p cetra-storage --lib -- -D warnings`.

---

### Phase QE6.2 — WAL Binary Framing, Memtable on `Batch`, `anchor`, `flush`, `lib.rs` WAL Boundary & Fuzz Seeds *(parallel with QE6.3, QE6.4)*

**Files:**
- `crates/storage/src/wal/{record.rs, replay.rs, writer.rs}`
- `crates/storage/src/memtable.rs`
- `crates/storage/src/anchor.rs`
- `crates/storage/src/flush.rs`
- `crates/storage/src/deletion.rs` (`project_snapshot` only)
- `crates/storage/src/engine/{mem_source.rs, traces_source.rs, diff_tests.rs}` (zero-copy reads directly from `MemBatch`'s `Arc<Batch>` and cached `Arc<Batch>` partials; update `Memtable::insert` test calls in `diff_tests.rs`!)
- `crates/storage/src/query/{mod.rs, traces.rs}` (2-line DF oracle bridge: `rows_for` returns `Vec<Arc<Batch>>` and converts inside `query/mod.rs::scan_union` with `Some(scan.schema.as_ref())` so `query/{spans,scores}.rs` need zero edits; `traces.rs` calls `column_to_record_batch(&p, Some(traces_schema().as_ref()))`)
- `crates/storage/src/lib.rs` (only WAL/append boundary: `append_spans`/`append_scores` convert `RecordBatch -> Batch` at entry; `recover_table`, `replay_batch`, `project_of` run on `Batch`)
- `crates/storage/src/fuzz_entry.rs` + `fuzz/corpus/wal_record/*` + `fuzz/corpus/wal_replay/*`

**Deliverables:**
1. **WAL binary framing (`wal/{record,replay,writer}.rs`):**
   - Preserve 17-byte outer WAL header and `FORMAT_VERSION = 1`.
   - `record::encode(seq, batch: &Batch, out)` uses `cetra_column::frame::encode_batch_with_limit` + `MAX_PAYLOAD` (256 MiB) check.
   - `record::read_batch(payload, table)` rejects `payload.len() > MAX_PAYLOAD`, calls `cetra_column::frame::decode_batch(payload, Some(&table.column_schema()), None)`, and **rejects any non-`Column::Flat` column** (anti-Const/Rle DoS bomb guard!). Delete all ~200 lines of Arrow IPC FlatBuffer/LZ4 guards and `arrow::ipc` imports.
   - `wal/replay.rs` and `wal/writer.rs` pass `cetra_column::Batch` and `batch.memory_bytes()`.
2. **`memtable.rs`, `anchor.rs`, `flush.rs`, `deletion::project_snapshot`:**
   - `MemBatch` stores `Arc<cetra_column::Batch>`, `bytes: batch.memory_bytes()`, `partials: Arc<OnceLock<Arc<cetra_column::Batch>>>`.
   - Multi-day split via `Bitmap` + `Batch::filter_mask` + `compact(None)`; `coalesce` via `Batch::concat`; `trace_partials` via `partials_from_spans_batch`.
   - `AnchorPartition` holds `Batch`; `Anchors::rebuild_file` calls `file_format::read_trace_anchors`.
   - `flush.rs` runs end-to-end on `Batch` + `cetra_query::tombstone` + `file_format::write_file_batch`.
   - `deletion::project_snapshot` uses `drop_covered_batch`.
   - Remove stale `#![allow(dead_code)]` directives in touched files.
3. **Zero-copy `engine` memtable reads + 2-line DF oracle bridge + WAL fuzz seeds:**
   - Update `fuzz_entry.rs` (`wal_record`, `wal_replay`) to `Batch` and run `just fuzz-seed`.

**Tests & Done Criteria:**
- Unit test verifying crafted `Column::Const(len = u32::MAX)` WAL payload is rejected cleanly in `wal::record::read_batch` without OOM/hang.
- All `wal::*`, `memtable::*`, `anchor::*`, `flush::*`, `crash_tests`, `resources/memory`, and `fuzz_entry` replays (including legacy Arrow IPC regressions cleanly returning `InvalidPayload`) pass.

---

### Phase QE6.3 — Compaction K-Way Merge, Slicing & GDPR-Sound `holds_covered` *(parallel with QE6.2, QE6.4)*

**Files:**
- `crates/storage/src/compaction/merge.rs`
- `crates/storage/src/compaction/slice.rs`
- `crates/storage/src/compaction/job.rs`
- `crates/storage/src/compaction/rewrite.rs`

**Deliverables:**
1. **`compaction/merge.rs`:**
   - Runs ALL `ParquetSource` stream pulls off Tokio worker threads (on `WorkerPool` / `spawn_blocking`).
   - Uses compaction's dedicated `GlobalPool(compaction.max_memory)` + `SpillManager::with_fixed_cap(PathBuf::new(), 0)` (no spill!), preserves `batch_rows` clamping, and holds `ScopedPool` `Reservation`s for each stream's active `Batch`, `pending` group tail, and chunk under construction.
   - Maps exclusive `Slice::upper` to inclusive `ScanSpec::time_range.max_ns = upper.checked_sub(1)` (short-circuiting empty on `i64::MIN`) alongside exact `time < upper` in `ScanSpec::filters`.
   - Streaming k-way min-heap merge ordered by `SortKey` (`Spans`: `(trace_id ASC, start_time ASC)`; `Scores`: `(created_at ASC)`) + `(input_index ASC, row_index ASC)` tie-breaker over inputs sorted by `file.id` ascending.
   - Emits chunks split on `same_group` boundary -> `drop_covered_batch` -> `latest_spans_batch` + `partials_from_spans_batch` (or `latest_scores_batch`) -> `spawn_blocking` `BatchFileWriter::write`.
2. **`compaction/slice.rs`:** calls `file_format::read_time_column` (zero `parquet/arrow`).
3. **`compaction/job.rs`:** uses `cetra_query::tombstone::covered_predicate`, `file_format::write_file_batch`, `Suspects` wired to `self.verify`, and deterministic `query_err_to_df` so `may_be_corrupt` queues all merge inputs (`quarantine_tests.rs:294`).
4. **`compaction/rewrite.rs` (`holds_covered` — GDPR-sound):**
   - Runs off Tokio worker thread (`WorkerPool` / `spawn_blocking`).
   - Constructs `ParquetSource` with **`deletions: Arc::from([])`** (inline tombstones disabled!), `projection: Some(key_indices)`, `time_range` open, `filters: vec![covered_predicate, ingest_time <= max_cutoff]`, and `id_lookups` populated **only** when every applicable deletion targets the exact same single column!
   - Stops immediately on first batch with `batch.num_rows() > 0`.
   - Privacy-safe error logs only; zero `datafusion` imports in `compaction/` (using `error.rs`'s bridge helper).

**Tests & Done Criteria:**
- Dedicated tests for: (a) `holds_covered` with disjunctive `User` deletion (`trace_id` OR `user_id` OR `session_id`) where only one disjunct matches (verifying zero GDPR false negatives!), (b) `holds_covered` where `start_time > cutoff` but `ingest_time <= cutoff`, (c) k-way merge UUIDv7 tie-order & tight 16 MiB `compaction.max_memory` (`compaction/merge.rs:770`), (d) `quarantine_tests::a_failing_merge_suspects_its_inputs` and `unwind::time_values_reports_a_panic_as_a_corrupt_read`.

---

### Phase QE6.4 — Streaming Media GC, `verify.rs` Off `parquet/arrow` & `golden_tests/dump.rs` *(parallel with QE6.2, QE6.3)*

**Files:**
- `crates/storage/src/media.rs`
- `crates/storage/src/verify.rs`
- `crates/storage/src/golden_tests/dump.rs`

**Deliverables:**
1. **`media.rs` (`MediaGc::live_set`):**
   - Short-circuit empty `HashSet` immediately if `state.projects.is_dropped(project)` is true.
   - Filter catalog `Spans` files by `keep_from = state.projects.keep_from(project).unwrap_or(NaiveDate::MIN)` (`keep_from..=NaiveDate::MAX`).
   - Run streaming `UnionSource` of `ParquetSource` (using `objects.store()`, `Suspects::new(Arc::clone(&self.verify))`, active tombstones) + `MemtableSource` (`project_snapshot` with `keep_from` & tombstones), projecting ONLY `[media_refs]` (`visible_from = None`), executing on `WorkerPool` / `spawn_blocking` under unbounded `GlobalPool::standalone(usize::MAX)`.
   - Stream `Batch`es directly (no full-RAM `collect()`) into `HashSet<String>` across `Flat`/`Const`/`Dict`/`Rle` list encodings; bridge `QueryError` into `MediaError::Query` via `error.rs`.
2. **`verify.rs`:** replace `ParquetRecordBatchStreamBuilder` in `verify_file` with `file_format::verify_parquet_file` (zero Parquet/Arrow decoding in `verify_file`; legacy `may_be_corrupt`/`is_store_error`/`Suspects::report` error classifiers remain until M7 deletes DataFusion).
3. **`golden_tests/dump.rs`:** replace `arrow::json::ArrayWriter` in `rows()` with a `cetra_column::Batch` JSON formatter matching exact `ArrayWriter` rules:
   - omits null keys (`explicit_nulls = false`);
   - trims trailing fractional zeros on timestamps (`"2026-02-10T10:00:00Z"`);
   - serializes `I64`/`F64`/`Decimal128` as JSON numbers;
   - serializes `FixedBytes8`/`FixedBytes16` as lowercase hex;
   - serializes `MapUtf8` as JSON objects.
   - Verified by a unit test asserting exact `serde_json::Value` equality against `arrow::json::ArrayWriter` across every golden column type before removing `arrow::json`.

**Tests & Done Criteria:**
- All `media::*` (including dropped project, `keep_from` retention, pending tombstones, corrupt file suspect reporting), `verify::*`, `verify_pass::*`, `quarantine_tests::*`, `s3_fault_tests::*`, and `golden_tests::*` (including `build_is_deterministic`) pass cleanly.

---

### Phase QE6.5 — Final Cleanup, Import Audit & Full Storage Verification

**Files:**
- `crates/storage/src/lib.rs` (drop compaction's legacy DataFusion `RuntimeEnv`)
- Delete temporary QE6.1 Arrow wrappers in `dedup.rs`, `partials.rs` (keeping only legacy `TraceMerger` if `query/traces.rs` still calls it until M7), `parquet.rs`

**Deliverables:**
1. Audit imports across `crates/storage/src/`: confirm **zero** DataFusion/Arrow/Parquet imports in `compaction/`, `media.rs`, `flush.rs`, `wal/`, `memtable.rs`, `anchor.rs`, `dedup.rs`, `verify.rs` (except `error.rs` public enum bridges, `file_format/`, `engine/parquet_*.rs`, and legacy M1–M6 oracle modules `query/`, `views/`, `reads/`, `sql/`, `column_bridge.rs` deleted in M7).
2. Run `CETRA_MODEL_CASES=8`, `crash_tests`, `quarantine_tests`, `resources/memory`, `golden_tests`, and all `fuzz_entry` replays.

---

### Phase QE6.6 — Neon Code Review, Fixes & M6 Closeout

**Deliverables:**
- Full Neon cold code review of all M6 changes.
- Fix every Blocker, Major, and Minor.
- Run full `just check-rust` + `just check-workflows` (green!), update `docs/roadmap.md` and `docs/plans/query-engine/design.md` for M6 complete.
