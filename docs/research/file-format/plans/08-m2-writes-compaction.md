# cetra workload · M2: writes and compaction

Implementation plan for milestone M2 of [08-cetra-workload-design.md](08-cetra-workload-design.md). Revision 3, after two cold reviews.

M2 measures what Strata costs to write in cetra's life cycle: a flush of a few files at once, the merge of L0 files into an L1 file, and where a Strata write spends its time. It adds two queries to `fmtbench cw` (`compact` and `flush:<n>`) and a command that cuts a case into more files (`cw split`). M2.1 and M2.2 are each done by one agent in one go, in order; each ends with the checks green and one commit. M2.3 is the runs and the report, done by the orchestrating session.

**Prerequisites:** M1.1 to M1.4 are committed (`fmtbench cw gen`, `import`, `run`, `one`, the `write` query).

**Read first:**
- [08-cetra-workload-design.md](08-cetra-workload-design.md): § Decisions (CW-7, CW-12, CW-16, CW-18, CW-20, CW-22), § M2.
- [08-m1-likely-failures.md](08-m1-likely-failures.md): § Conventions, § Build, and the contracts of M1.1 and M1.2 (`ProcResult`, `Summary`, `losses`, `write_format`, the case directory).
- `bench/src/cw/harness.rs`: `one`, `run_cmd`, `losses`, `print_line`, `write_format`, `read_inputs`, `concat_file`, `proc_status_bytes`. M2.2 follows `write_format`'s shape.
- `bench/src/cw/cases.rs`: `write_inputs`, how input files are cut and sorted.
- `bench/src/main.rs`: `write_parquet` (the `pq_cetra` row-group rule: flush at 64 MiB in progress or 100,000 rows), `parquet_props`, `write_strata`, `open_strata`, `location`, `merge_pq` (a non-streaming merge M2 does not use).
- `strata/src/merge.rs`: `strata::merge(inputs, sort_key, config)`, which copies payload stores when it can.
- spec 03 § 5 (Compaction): inputs are sorted runs, merged streaming, one batch per input at a time; § 2 (Flush): up to `flush.upload_concurrency` (8) files at once, 256 MiB `flush.max_bytes`.

**Conventions:** those of M1. All paths below are under `docs/research/file-format/`.

## What it is, and what it is not

**When M2 is done:**
- `fmtbench cw split <dir> <n>` cuts a case's input into `n` files, as `n` L0 files of one time range each.
- `cw run … --queries write,compact` measures the merge of a case's files into one, per format; `flush:<n>` measures `n` writers flushing at once.
- M2.3's runs give M2's verdicts and a breakdown of Strata's write time, written up in `strata-readiness.md`.

**Not in M2:**
- rebuilding `traces` partials during compaction (CW-20);
- deduplicating `(trace_id, span_id)` during the merge, which cetra does (spec 03 § 5): M2's cases have no duplicates, so both formats would do no work for it;
- day partitions and many files (M3), deletion (out of scope, CW-6).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| CW-16 | A write wins only on both wall time and CPU time. |
| CW-12 | Strata must also write smaller files and use less peak memory. |
| CW-18 | One fresh process per query, format and round. |
| CW-20 | M2's compaction is the k-way merge of the `spans` files, per format. |
| CW-22 | The write breakdown comes from profiles, grouped by function. |

## Versions

No new dependency. `arrow::row::{RowConverter, SortField, OwnedRow}` and `arrow::compute::interleave_record_batch` are in arrow 59.3.2, a dependency of the bench already (the bench doesn't use these two yet; `strata/src/merge.rs` uses `RowConverter`).

## Phase graph

| Phase | Edit | Depends on |
|---|---|---|
| M2.1 split | `bench/src/cw/cases.rs`, `bench/src/cw/mod.rs`, `bench/src/main.rs` (`USAGE`) | M1.4 |
| M2.2 compact and flush | `bench/src/cw/writes.rs` (new), `bench/src/cw/mod.rs`, `bench/src/cw/harness.rs` | M2.1 |
| M2.3 runs and report | `strata-readiness.md`, `plans/08-cetra-workload-design.md` (orchestrator) | M2.2 |

Both phases edit `cw/mod.rs`, so they run one after the other.

---

## M2.1: split

**Goal:** cut a case's input into `n` input files, each one time slice sorted by `(trace_id, start_time)`, as `n` L0 files of consecutive flushes.

**Edit:** `bench/src/cw/cases.rs`, `bench/src/cw/mod.rs` (the `"split"` and `"flush-input"` arms), `bench/src/main.rs` (`USAGE` only: add ` | cw split <dir> <n> | cw flush-input <dir>`).

### Contracts

```text
fmtbench cw split <dir> <n>
fmtbench cw flush-input <dir>
```

```rust
/// `cw split <dir> <n>`: rewrites `<dir>/input/` as `n` files (n >= 1).
pub fn split_cmd(args: &[String]) -> crate::R<()>;

/// The rows of `batches` (one schema) cut into `n` slices of consecutive `start_time`, each
/// sorted by `(trace_id, start_time)`: slice i holds the rows of global ranks
/// [len*i/n, len*(i+1)/n) by `start_time` (ties by input order), and empty slices are dropped.
pub fn split_batches(batches: &[RecordBatch], n: usize) -> crate::R<Vec<RecordBatch>>;

/// Bytes of Arrow memory one flush writes (spec 03 § 2, `flush.max_bytes`).
pub const FLUSH_BYTES: usize = 256 << 20;

/// `cw flush-input <dir>`: writes `<dir>/flush.arrow`, one batch of about `FLUSH_BYTES`: the
/// first rows of the input files in order (the earliest time slices), sorted by
/// `(trace_id, start_time)`. M2.2's `flush:<n>` reads only this file, so its processes don't
/// load the whole case.
pub fn flush_input_cmd(args: &[String]) -> crate::R<()>;
```

### Steps

1. `split_cmd`: parse `<dir> <n>` (`n` from 1 to 1,000, else an error). Read every `input/NNN.arrow` in name order with `arrow::ipc::reader::FileReader`, keeping every batch.
2. `split_batches`:
   1. Collect `(start_time, batch index, row index)` for every row (`start_time` is `TimestampNanosecond`; read it as `i64` values).
   2. Sort by `(start_time, batch index, row index)`.
   3. For each slice, gather its rows with `arrow::compute::interleave_record_batch(&batch_refs, &indices)` (slices are non-empty once empty ones are dropped; an input with no rows is the error `no input rows`). An `ArrowError::OffsetOverflowError` from it (a string column of the slice passes the 2 GiB offset limit) becomes `slice <i> of <n> is too large for one batch: split into more files`; other errors are returned as they are. `start_time` is `Timestamp(Nanosecond, Some("UTC"))`: downcast to `TimestampNanosecondArray`.
   4. Sort the slice by `(trace_id, start_time)` with `lexsort_to_indices` and `take_record_batch`, as `write_inputs` does.
3. Write the slices: remove any `input.old-*` directory left by an earlier failed run, move `input/` to `input.old-<process id>/`, write `input/000.arrow` … one batch each, then remove `input.old-<process id>/` once every new file is written. Remove `pq_cetra/`, `st/`, `compact-pq_cetra/`, `compact-st/`, `flush-pq_cetra/`, `flush-st/` and `flush.arrow` if present: they come from the old cut. The text files (`cw_*.txt`) stay. Peak memory is about twice the case (the inputs and the slices); fine on ovh-us's 62 GB for every M2 case.
4. Print the row count and each file's rows and `start_time` range.
5. **`flush_input_cmd`**: read the input files in name order with `FileReader`; `total_rows` and `total_bytes` are the sums of `num_rows()` and `get_array_memory_size()` of the batches as read (fresh from IPC, so the sizes are their own). `rows = flush_rows(total_rows, total_bytes, FLUSH_BYTES)`, a pure function: `min(total_rows, ceil(total_rows * limit / total_bytes))` in `u128` arithmetic, and `total_rows` when `total_bytes` is 0. Take the first `rows` rows across the batches in order (`slice` then `concat_batches`), sort by `(trace_id, start_time)`, and write `<dir>/flush.arrow` with one batch. Print `rows` and the batch's `get_array_memory_size()` after the sort.

### Tests (`cw/cases.rs`)

- `split_keeps_every_row_in_time_slices`: `gen_spans("text_unique", Some(200))` written with `write_case` to a temp dir, then `split_batches` of its input batches with `n` = 1, 3 and 10: the row counts add up; each slice is sorted by `(trace_id, start_time)`; slice i's largest `start_time` is at most slice i+1's smallest; the multiset of `(trace_id, span_id)` equals the input's.
- `split_drops_empty_slices`: a batch of `text_unique` spans cut with `.slice(0, 3)`, `n` = 10: 3 slices of 1 row.
- `flush_rows_scales_to_the_limit`: `flush_rows(1_000, 4_000, 1_000)` = 250; `flush_rows(1_000, 4_001, 1_000)` = 250 (ceil of 249.94); `flush_rows(10, 100, 1_000)` = 10; `flush_rows(10, 0, 1_000)` = 10.
- `flush_input_takes_the_first_rows`: on a `text_unique --traces 200` case (smaller than `FLUSH_BYTES`), `flush.arrow` holds every row, sorted by `(trace_id, start_time)`.
- `split_cmd_rewrites_the_input`: `split_cmd([dir, "6"])` on a `text_unique --traces 100` case with `pq_cetra/` and `flush.arrow` present: `input/` holds 6 files, `pq_cetra/` and `flush.arrow` are gone, the `cw_*.txt` files are unchanged, and no `input.old-*` directory is left.

Tests use a local `temp_dir(name)` helper in the test module, following M1's convention (`std::env::temp_dir()`, `cw-test-<name>-<process id>`, removed first).

### Done when

In `bench/`, with `LIBCLANG_PATH=/usr/lib/llvm-18/lib`:
- `cargo build --release` and `cargo test --release cw::` pass.
- `cargo run --release -- cw gen text_unique /tmp/cw-s --traces 400` then `cargo run --release -- cw split /tmp/cw-s 10` prints ten files with increasing time ranges.

**Commit:** `Add fmtbench cw split: a case as n L0 files`

---

## M2.2: compact and flush

**Goal:** two queries of the write kind: `compact`, the merge of a case's files into one per format, and `flush:<n>`, `n` writers flushing at once.

**Edit:** `bench/src/cw/writes.rs` (new), `bench/src/cw/mod.rs` (`pub mod writes;`), `bench/src/cw/harness.rs`.

### Contracts

```rust
// writes.rs
use crate::cw::harness::{ProcResult, proc_status_bytes, check_format};

/// Merges the Parquet files `inputs`, each sorted by `(trace_id, start_time)`, into one file
/// at `out` with cetra's Parquet settings, streaming in batches of 8,192 rows. The synchronous
/// reader loads one row group's column chunks per input at a time (`pq_cetra` row groups are
/// up to 64 MiB or 100,000 rows), so memory is about one row group per input, as cetra's
/// streaming merge holds one batch per input (spec 03 § 5). Rows with equal keys keep their
/// input order, then their row order. Returns (rows, bytes written).
pub fn merge_parquet(inputs: &[PathBuf], out: &Path) -> crate::R<(u64, u64)>;

/// `compact` for `format` on the local case `dir`: merges `<dir>/<format>/*` into
/// `<dir>/compact-<format>/000.<ext>`, `runs` times. The `write` query must have run first.
pub async fn compact_format(dir: &Path, format: &str, runs: usize) -> crate::R<ProcResult>;

/// `flush:<writers>` for `format` on the local case `dir`: `writers` threads each write the
/// batch of `<dir>/flush.arrow` (M2.1's `flush-input`) to `<dir>/flush-<format>/wNN.<ext>` at
/// once, `runs` times. Every writer writes the same rows; formats keep no state across files,
/// so this costs what `writers` different projects' flushes would.
pub fn flush_format(dir: &Path, format: &str, writers: usize, runs: usize) -> crate::R<ProcResult>;

// harness.rs

/// Queries that write files: `write`, `compact` and `flush:<n>`. They run locally only, take
/// no warm-up, and are judged on CPU time and bytes written too (CW-16, CW-12).
pub(crate) fn is_write_like(query: &str) -> bool;
```

In `harness.rs`, `proc_status_bytes` and `check_format` become `pub(crate)` (they are private today); `ProcResult` is already `pub`.

`ProcResult` fields, as `write_format` sets them:

| Query | `rows` | `answer` | `bytes_written` | `tail_bytes` |
|---|---|---|---|---|
| `compact` | output rows | hex of `xxh3_64` of the output's `(trace_id, start_time, span_id)` sequence in file order (16 + 8 little-endian + 8 bytes a row), read back from the output after the timed runs, so a different order or row set is an answer mismatch | output file size | `null` |
| `flush:<n>` | rows of the batch × `n` | hex of `xxh3_64(b"")` | the `n` files' sizes | `null` |

`gets`, `bytes` = 0, `waves` = `null`; `rss_start` and `vmhwm` as `write_format` reads them, `vmhwm` right after the timed runs.

### Steps

1. **`merge_parquet`.**
   1. Open each input with `parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(File::open(p)?)?.with_batch_size(8192).build()?`. The total row count is the sum of `builder.metadata().file_metadata().num_rows()`, read before `build()`.
   2. A `RowConverter::new(vec![SortField::new(trace_id type), SortField::new(start_time type)])` from the first input's schema.
   3. Inputs whose file has no rows are skipped (as `strata::merge` skips empty inputs). Each remaining input keeps `batches: Vec<RecordBatch>`, the batches it read since the last output flush (its current batch last), its current batch's converted rows (`convert_columns` on the two key columns) and nothing else. The heap is a `BinaryHeap<Reverse<(OwnedRow, usize, usize)>>` of (key row, input index, row index in that input's current batch); each input has at most one entry. Ties order by input index, then row, so equal keys keep their input order.
   4. Loop: pop the smallest entry `(_, i, r)`; append `(i, generation, r)` to the pending list, where `generation` is the index of input i's current batch in its `batches`. Then advance input i: if `r + 1` is in its current batch, push row `r + 1`; otherwise read its next batch (if any), push it to `batches`, convert its keys and push its row 0. When the pending list reaches 8,192 rows (or the heap is empty), flush: list the distinct `(input, generation)` pairs of the pending list as slots, call `interleave_record_batch(&slot_refs, &(slot, row) pairs)`, write the batch, clear the pending list, and truncate every input's `batches` to its current (last) batch, whose generation becomes 0. The pending list is never empty at a flush.
   5. Write with `ArrowWriter::try_new(file, schema, Some(crate::parquet_props("pq_cetra", &schema, total_rows)))`, and after each `write` flush the row group when `in_progress_size() >= 64 << 20` or `in_progress_rows() >= 100_000`, as `crate::write_parquet` does for `pq_cetra`. Close, and return the rows and the file size.
2. **`compact_format`.** Read `rss_start`. Check that `<dir>/<format>/` holds files, else the error `run the write query first`. Then `runs` times, timed with `Instant` and `ProcessTime`:
   - `pq_cetra`: delete and recreate `<dir>/compact-pq_cetra/`, then `merge_parquet(<dir>/pq_cetra/*.parquet in name order, <dir>/compact-pq_cetra/000.parquet)`;
   - `st`: delete and recreate `<dir>/compact-st/`, open the files with `crate::open_strata(&crate::location(dir.to_str().ok_or("non-UTF-8 path")?)?, "st")`, `strata::merge(&files, &["trace_id", "start_time"], strata::Config::default())`, and write the bytes to `<dir>/compact-st/000.strata`. The open is inside the timed region, as Parquet's reader opens its inputs inside it.
   - Both end with `File::open(out)?.sync_all()` inside the timed region: cetra's compaction makes each output durable (spec 03 § 5).
   
   `Config::default()` is what an L0 → L1 merge uses; `Config::cold` is for rewriting cold files, not for these merges. What is measured is each format's merge as it is: `strata::merge` is not streaming (it reads the key columns whole, keeps a 16-byte row-source map, builds the file in memory) and compresses on every core, and it copies payload stores when the inputs allow it, while `merge_parquet` streams on one thread and re-encodes every column. Read `vmhwm` after the runs, then compute the `answer` by reading the output back, outside the timed region: Parquet with `ParquetRecordBatchReaderBuilder` and a `ProjectionMask::roots` of the three columns; Strata with `crate::open_strata(&location, "compact-st")` and `strata::read_column(&file, file.schema().index_of(name)?)` for each of the three. The bytes hashed per row are `FixedSizeBinaryArray::value(i)` of `trace_id` (16 bytes), `TimestampNanosecondArray::value(i).to_le_bytes()` of `start_time` (8), and `FixedSizeBinaryArray::value(i)` of `span_id` (8). `rows` is `merge_parquet`'s count for Parquet and the `FileSummary::rows` that `strata::merge` returns for Strata.
3. **`flush_format`.** Read `rss_start`. Read `<dir>/flush.arrow` (written by `cw flush-input`; its absence is the error `run cw flush-input first`) into one batch: the only data the process loads. Then `runs` times: delete and recreate `<dir>/flush-<format>/`; start the clocks; in `std::thread::scope`, `writers` threads each call `crate::write_parquet("pq_cetra", &batch, wNN.parquet)` or `crate::write_strata(&batch, wNN.strata)` and then `File::open(path)?.sync_all()` (a flush is durable before it commits, spec 03 § 2); join; stop the clocks; sum the file sizes. Read `vmhwm` after the runs, then remove `<dir>/flush-<format>/`, outside the timed region. Strata's writer compresses on every core (`Config::default().write_threads`), as cetra's flush would; that is part of what is measured. The writers share one input batch, so `vmhwm` holds one 256 MiB batch where `n` projects would hold `n`: the figure compares the writers' own memory (Strata's `write_strata` builds the whole file in memory before writing it; Parquet's writer streams), not cetra's whole flush. M2.3's report says so.
4. **`harness.rs`.**
   - `one`: `"compact"` → `writes::compact_format`; `"flush:<n>"` (n from 1 to 64, else an error) → `writes::flush_format`; `"write"` as before.
   - `is_write_like`, used where `query == "write"` is tested today: `run_cmd` skips write-like queries on an `s3://` location; `losses` and `print_line` compare CPU and size for them.
   - Answer comparison: every query except `write` and `flush:<n>` (whose answers are constant).
   - The compared-figures text of the verdict: `median, p95, rss, cpu, size` for write-like queries.

### Tests

`writes.rs` (temp dirs; inputs built from `World` traces with `spans_to_record_batch`, cut by `start_time` and sorted by `(trace_id, start_time)`, written with `crate::write_parquet("pq_cetra", …)`):
Tests build their inputs with a local helper `pq_inputs(dir, spans, n)` in `writes.rs`'s test module: `spans_to_record_batch`, cut into `n` slices by `start_time`, each sorted by `(trace_id, start_time)` and written with `crate::write_parquet("pq_cetra", …)` to `NNN.parquet`.

- `merge_parquet_merges_sorted_files`: 3 input files: the output has every input row, sorted by `(trace_id, start_time)`; its `(trace_id, span_id)` multiset equals the inputs'.
- `merge_parquet_keeps_input_order_on_ties`: one `World` span cloned twice with the same `trace_id` and `start_time` and different `span_id`s (`SpanId::from_bytes`), one per input file: the output holds input 0's row first.
- `merge_parquet_crosses_batch_boundaries`: 2 inputs of 20,000 rows each whose keys interleave (`World` spans with payloads stripped by `crate::strip_payloads`, alternated between the inputs after sorting): the output is sorted, holds every row once, and its `(trace_id, span_id)` multiset equals the inputs'.
- `merge_parquet_streams`: 4 inputs of 30,000 rows with payloads stripped: the output has 120,000 rows in two row groups or more (100,000-row rule).
- `merge_parquet_skips_empty_inputs`: one of three inputs has 0 rows: the output holds the other two's rows.
- `compact_agrees_across_formats`: `cases::gen_spans("text_unique", Some(60))` written with `cases::write_case` (four input files, so a 4-way merge), both formats written with `harness::write_format`, then `compact_format` for each: equal `rows` and `answer`, and each output file exists.
- `flush_writes_one_file_per_writer`: after `cases::flush_input_cmd`, `flush_format(dir, f, 3, 1)` for each format: `bytes_written` equals 3 × the size of one `crate::write_parquet("pq_cetra", …)` or `crate::write_strata` of the same batch to a separate file (both writers are deterministic), `rows` = 3 × the batch's rows, and `flush-<format>/` is removed afterwards. Test modules have their own `temp_dir` helper, as in M2.1.

`harness.rs`:
- `write_like_queries`: `is_write_like` is true for `write`, `compact`, `flush:8`, false for `scan_payload` and `search:bug`; `losses("compact", …)` with more Strata CPU gives `["cpu"]`.

### Done when

In `bench/`, with `LIBCLANG_PATH=/usr/lib/llvm-18/lib`:
- `cargo build --release` and `cargo test --release cw::` pass.
- `cargo run --release -- cw gen text_unique /tmp/cw-m2 --traces 400`, `… cw split /tmp/cw-m2 10`, `… cw flush-input /tmp/cw-m2`, `… cw run /tmp/cw-m2 --queries write,compact,flush:2 --rounds 2 --runs 1` finish with no `ANSWER MISMATCH` and a verdict for each of the three queries. (`write` comes first in the list because `split` removed the written formats; at this size the flush batch is the whole input.)

**Commit:** `Add fmtbench cw compact and flush: merges and concurrent writers`

---

## M2.3: runs and report (orchestrator)

On ovh-us, idle, after M1.5:
1. **Compaction.** For `swe`, `wildchat`, `text_unique` and `b64`: `cw split <dir> 10`, then `cw run <dir> --queries write,compact --rounds 26 --runs 2`. If `wildchat` fails on the 2 GiB slice limit, split it into 20.
2. **Concurrent flushes.** For `swe` and `wildchat`: `cw flush-input <dir>`, then `cw run <dir> --queries flush:4,flush:8 --rounds 26 --runs 2`.
3. **Write breakdown.** For `swe`, `wildchat`, `b64` and `text_unique`: `perf record -F 999 -g -o <file> fmtbench cw one <dir> st write 3`, then `perf report --no-children --sort symbol` grouped by module path: `strata::payload::chunk` (content-defined chunking), `strata::payload::gram` (n-gram index), `strata::payload::{store,write,refs}` (payload store and references), `strata::narrow` (narrow encodings), `ZSTD_*` / `zstd` (compression), the rest. The same for `pq_cetra` with `parquet::` and `ZSTD_*`.
4. Write M2's verdicts and the breakdown in `strata-readiness.md` § M2, with what `flush:<n>` measures (one shared batch) and whether `strata::merge` copied the payload stores (profile one `compact` per format). Update the design doc's § M2: rebuilding `traces` moved to M3 (CW-20), and the payload carry-over question is answered by that profile. A loss gets a profile and a fix plan (CW-5).

## Unverified

- Whether `strata::merge` copies the payload stores of these inputs (its `MergeCopy` path) or re-encodes them: M2.3 profiles one `compact` per format and reports which, since it decides much of the verdict.
- `flush:<n>` with Strata's writer on every core per writer oversubscribes the CPU at `n = 8`; that is how `Config::default()` behaves, and the CPU figure shows the cost.
