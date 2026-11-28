# cetra workload · M3: many files and small files

Implementation plan for milestone M3 of [08-cetra-workload-design.md](08-cetra-workload-design.md). Revision 3, after two cold reviews.

M3 builds projects the way cetra builds them, with `cetra-storage` itself writing the Parquet side (CW-21), converts each data file to Strata, and measures what many files and small files cost: cold lookups that probe every file, a 30-day rollup, the memory open files hold, the startup anchor rebuild, and the size and open cost of small L0 files of all three tables (CW-25). M3.1 and M3.2 are each done by one agent in one go, in order; each ends with the checks green and one commit. M3.3 is the runs and the report, done by the orchestrating session.

**Prerequisites:** M2 is committed (`fmtbench cw` with `write`, `compact`, `flush:<n>`, `split`, `flush-input`).

**Read first:**
- [08-cetra-workload-design.md](08-cetra-workload-design.md): § Decisions (CW-7, CW-8, CW-9, CW-12, CW-13, CW-18, CW-21, CW-23, CW-24, CW-25), § M3.
- [08-m1-likely-failures.md](08-m1-likely-failures.md): § Conventions, § Build, the M1.2 and M1.3 contracts.
- spec 03 § Layout, § Trace anchor (the startup rebuild), § 2 Flush, § 3 Parquet files, § 5 Compaction, § 6 `SpansProvider::scan`.
- `crates/storage/src/lib.rs`: `Storage::open` (it spawns a background compaction task on the wall clock), `append_spans`, `append_scores`, `flush_now`, `compact` (returns `CompactionReport { jobs, failed, … }`, `crates/storage/src/compaction/job.rs:79`), `collect_obsolete`, `shutdown`; the test helper `config_in` (~line 2058) and a test that sets `config.compaction.interval` (~line 3741). `crates/storage/src/config.rs` (the sub-config types are `pub` in `cetra_storage::config`). `crates/storage/src/error.rs` (`AppendError::Backpressure`). `crates/storage/src/objects.rs` ~line 283 (object names `<level>-<uuid>.parquet`, level `0` or `1`).
- `crates/core/src/{score.rs,enums.rs,batch.rs,schema.rs,span.rs}`.
- `bench/src/cw/{harness.rs,read.rs,cases.rs,writes.rs}`, `bench/src/pqfast.rs`, `bench/src/main.rs` (`open_strata`, `strip_payloads`), `testdata/src/datagen.rs`.
- The `strata` API: `StrataWriter`, `StrataFile::open`, `may_contain`, `lookup_scan`, `scan_batches`, `int_range`, `row_count`.

**Conventions:** those of M1. Paths are under `docs/research/file-format/` unless they start with `crates/`. Async tests use `#[tokio::test(flavor = "multi_thread")]`.

## What it is, and what it is not

**When M3 is done:**
- `fmtbench cw project <dir> …` writes a cetra project with `cetra-storage` and lays out its data files as Parquet (hard links) and as Strata conversions.
- `fmtbench cw layout <dir>` prints file counts and sizes per table, level and format.
- New `cw` queries on a project: `lookup_cold`, `lookup_around`, `rollup_30d`, `rollup_30d_cold`, `open:<table>`, `anchor_rebuild`.
- M3.3's runs give M3's verdicts on a 30-day project (small for A/B runs, 30M traces for verdicts) and on a small-files project, locally and on R2.

**Not in M3:** the query shapes of spec 05 (M4); concurrency and the read cache (M5); the write time of `traces` and `scores` small files (CW-25).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| CW-8, CW-13 | 1M traces a day for 30 days for verdicts; a small project for A/B runs. |
| CW-9 | 1–3 scores per trace. |
| CW-21 | `cetra-storage` writes the Parquet project; each file is converted to Strata. |
| CW-23 | `World` traces without payloads; 2% get real payloads; dated from 2027-01-04; closed days compacted explicitly; six live hours across a midnight left as L0. |
| CW-24 | Cold queries open their files inside each run, with no warm-up. |
| CW-25 | Small files come from cetra's engine flushing small batches. |

## Versions

The orchestrator adds the three dependencies to `bench/Cargo.toml` and `bench/Cargo.lock` before M3.1 starts; M3.1 doesn't edit either file.

| Crate | Version | Where | Why |
|---|---|---|---|
| `cetra-storage` | `{ path = "/home/sebastien/travaux/llm-observer/crates/storage" }`, the same checkout as `cetra-core` | `bench/Cargo.toml` | writes the Parquet projects (CW-21) |
| `uuid` | `{ version = "1.29", features = ["v7"] }`, the workspace pin | `bench/Cargo.toml` | `Score::score_id` |
| `bytesize` | `"2.7.0"`, the workspace pin | `bench/Cargo.toml` | `StorageConfig::min_free_bytes` |

**Paths.** `cetra-core` and `cetra-storage` must name the same checkout, or the bench builds two `cetra-core` crates whose types don't match. The committed `bench/Cargo.toml` names the main checkout for both (`/home/sebastien/travaux/llm-observer/crates/core` and `…/crates/storage`); the working copy is swapped to this worktree by the orchestrator while building, and the commit must carry the main-checkout paths. `cetra-storage` brings `datafusion` 55.2, `arrow`/`parquet` 59 and `object_store` 0.13, the bench's own pins: `cargo tree -d -e normal` must show one version each of those four (the orchestrator checked: `cetra-storage` adds second versions of small crates only, such as `digest`, `sha2`, `hashbrown` and `sqlx-core`). The implementer reports the new `bench/Cargo.lock` entries.

## Phase graph

| Phase | Edit | Depends on |
|---|---|---|
| M3.1 project | `bench/src/main.rs` (`USAGE`), `bench/src/cw/mod.rs`, `bench/src/cw/cases.rs` (make `input_paths` `pub(crate)`), `bench/src/cw/project.rs` (new) | M2 |
| M3.2 many-file queries | `bench/src/main.rs` (extract `strata_config`), `bench/src/cw/mod.rs`, `bench/src/cw/harness.rs`, `bench/src/cw/many.rs` (new) | M3.1 |
| M3.3 runs and report | `strata-readiness.md`, `plans/08-cetra-workload-design.md` (orchestrator) | M3.2 |

---

## M3.1: project

**Goal:** cetra projects written by `cetra-storage`, laid out as Parquet and Strata files, with their text files.

**Edit:** `bench/src/main.rs` (`USAGE`: ` | cw project <dir> --days D --traces-per-day N [--live-hours H] [--payloads <case-dir>,…] | cw project <dir> --small R --flushes K | cw layout <dir>`), `bench/src/cw/mod.rs` (`pub mod project;`, the `"project"` and `"layout"` arms), `bench/src/cw/cases.rs` (`input_paths` becomes `pub(crate)`, nothing else), `bench/src/cw/project.rs` (new).

### Contracts

```text
fmtbench cw project <dir> --days D --traces-per-day N [--live-hours H] [--payloads <case-dir>,<case-dir>]
fmtbench cw project <dir> --small R --flushes K
fmtbench cw layout <dir>
```

Options may come in any order; `--payloads` takes one comma-separated value. Defaults: `--live-hours 6`, no payloads. `<dir>` must not exist. The second form is the small-files project (CW-25).

```rust
/// Share of traces that get real payloads (CW-23).
pub const PAYLOAD_SHARE: f64 = 0.02;
/// Spans per `append_spans` call, as cetra's `trace_bench`. A trace may be split between calls.
pub const APPEND_SPANS: usize = 10_000;
/// Real payload pairs kept from the `--payloads` cases, and their byte budget.
pub const PAYLOAD_POOL: usize = 100_000;
pub const PAYLOAD_POOL_BYTES: usize = 512 << 20;
/// Strata conversions running at once for files under `CONVERT_ALONE_BYTES`; larger files are
/// converted one at a time (`StrataWriter` keeps every batch until `finish`).
pub const CONVERT_AT_ONCE: usize = 4;
pub const CONVERT_ALONE_BYTES: u64 = 64 << 20;
/// The first day of a project (2027-01-04T00:00:00Z), in the future so that the storage's
/// wall-clock background compaction never closes a day (CW-23).
pub const PROJECT_START: i64 = 1_799_020_800 * 1_000_000_000;

/// `cw project …`.
pub async fn project_cmd(args: &[String]) -> crate::R<()>;
/// `cw layout <dir>`: prints `layout_report`.
pub fn layout_cmd(args: &[String]) -> crate::R<()>;
/// Per table and level (`0` or `1`, the object name's prefix before `-`), the file count,
/// total and median size per format, and Strata / Parquet, as text.
pub fn layout_report(dir: &Path) -> crate::R<String>;
```

**The project directory:**

| Path | Content |
|---|---|
| `engine/` | `cetra-storage`'s data directory |
| `pq_cetra/<table>/day=<YYYY-MM-DD>/<level>-<uuid>.parquet` | a hard link to each active data file under `engine/objects/tables/<table>/project=<id>/day=<…>/` |
| `st/<table>/day=<YYYY-MM-DD>/<level>-<uuid>.strata` | the same rows, converted |
| `cw_traces.txt` | 20 trace ids, hex |
| `cw_around.txt` | the same ids' root `start_time`, nanoseconds, one per line |
| `cw_clock.txt` | the largest span `ingest_time` written, nanoseconds (the anchor rebuild's clock) |
| `cw_queries.txt` | 30-day project: `open:spans`, `open:traces`, `open:scores`, `lookup_cold`, `lookup_around`, `rollup_30d`, `rollup_30d_cold`, `anchor_rebuild`. Small-files project: `open:spans`, `open:traces`, `open:scores`, `lookup_cold`, `rollup_30d_cold`. |

### Steps

1. **Engine.** A `StorageConfig` built as `config_in` builds it (each sub-config at its `Default`, `min_free_bytes: ByteSize::b(0)`, `object_store: None`, `delete_objects: true`, `data_dir = <dir>/engine`), at their defaults: the background compaction task (it runs every `compaction.interval`, 30 s by default, at the wall clock, and the interval may not exceed 1 h) only does the count-based L0 merges cetra does during ingestion, because the project is dated in the future (`PROJECT_START`) and no day can close before its end + 2 h. `Storage::open` it. The project id is the first generated span's `project_id`.
2. **Payload pool** (30-day form). For each `--payloads` case, for each path of `cases::input_paths(&case)?`, read its batches one at a time with `arrow::ipc::reader::FileReader` and collect `(input, output)` pairs from rows where both are non-null, in order; stop reading as soon as the pool has `PAYLOAD_POOL` pairs or `PAYLOAD_POOL_BYTES` of strings.
3. **Traces.** `World::new(7)`, `Rng::new(42)`. Day `d` starts at `day_start(d) = PROJECT_START + d * 86_400 s`. One trace is generated into its own `Vec<Span>` (`let mut t = Vec::new(); world.trace(&mut rng, &mut t);`; its root is `t[0]`), then `crate::strip_payloads` on each of its spans; with `rng.unit() < PAYLOAD_SHARE` and a non-empty pool, every `SpanKind::Llm` span of the trace takes the next pool pair (cycling) as `input`/`output`, with `input_format`/`output_format = Some(PayloadFormat::Text)` and previews from `datagen::preview`. The trace is then placed at a target time `at`: `delta = at − t[0].start_time`; add `delta` to every span's `start_time`, `end_time` and every event's `time`; set every span's `ingest_time` to the trace's largest `end_time` + 1 s. Scores: 1 to 3 (`rng.range(1, 3)`), each `Score { project_id, score_id: Uuid::now_v7(), target: ScoreTarget::Trace { trace_id }, session_id and user_id from t[0], name: ["relevance", "toxicity", "helpfulness"][k % 3], value: ScoreValue::Numeric(rng.unit()), reason: None, source: ScoreSource::LlmJudge, evaluator_id, rule_id, experiment_id, dataset_item_id, author_user_id: None, metadata: empty, created_at = updated_at = ingest_time = the trace's ingest_time + 1 s, deleted: false }`.
4. **Append, streaming.** Each placed trace's spans and scores are moved into a span buffer and a score buffer. Whenever a buffer holds `APPEND_SPANS` items or more, its first `APPEND_SPANS` become one batch (`spans_to_record_batch`, `scores_to_record_batch`) and are appended, so a trace may be split between two batches; at the end of a form, the rest is appended. `append_spans` takes the batch by value: `AppendError::Backpressure` waits 100 ms and retries with a clone of the same batch; any other error stops the command.
5. **30-day form.** Days `0 … D−1`: `N` traces each at `at = day_start(d) + rng.below(86_400) s`, appended. Then `flush_now(Duration::from_secs(600))` and compaction passes `compact(now)` with `now = day_start(D) + 3 h`, repeated while a pass reports `jobs + failed > 0`, at most 100 passes (more is an error naming the last report), then `collect_obsolete(Duration::ZERO)`. Then the live part: `N * H / 24` traces at `at` uniform in `[day_start(D) − (H − 1) h, day_start(D) + 1 h)`, so the live traces cross the midnight that starts day `D`, as the anchor rebuild needs. Append them and `flush_now`; no explicit compaction: they stay L0 files (the ones before midnight are late data in day `D − 1`), unless the background task merges eight of them, as cetra would.
6. **Small-files form.** `--small R --flushes K`: `K` times, traces at `at = PROJECT_START + i s` (`i` counts traces from 0) until the flush's `R` spans are generated, appended, then `flush_now`. Each flush writes one small L0 file per table. No compaction. If `i` would pass 86,399, stop with the error `too many spans for one day: lower --small or --flushes`.
7. **Text files.** `cw_traces.txt`: the first `ceil(20 / m)` traces generated on each of `m = min(10, days with traces)` days spread evenly over them, cut to 20 (the small form counts as one day); `cw_around.txt` their root's `start_time` after placing; `cw_clock.txt` the largest span `ingest_time`.
8. **Layout.** `shutdown`, then every `*.parquet` file under `engine/objects/tables/` (after `collect_obsolete`, every such file is active; the small form has no obsolete files): create `pq_cetra/<table>/day=<…>/` and `st/<table>/day=<…>/`, hard-link the file to `pq_cetra/<table>/day=<…>/<name>`, and convert it: files of `CONVERT_ALONE_BYTES` or more one at a time, smaller ones up to `CONVERT_AT_ONCE` at once: read it with `ParquetRecordBatchReaderBuilder` (batch size 8,192), write each batch to a `StrataWriter` with `Config::default()` and the sort key of its table (`spans`: `["trace_id", "start_time"]`; `traces`: `["start_time", "trace_id"]`; `scores`: `["created_at"]`, as `crates/storage/src/parquet.rs` sorts them), `finish`, and write `st/<table>/day=<…>/<stem>.strata`.
9. Print per table: file count, rows, and bytes per format, and the time ingestion, compaction and conversion took.

### Tests (`cw/project.rs`)

- `small_project_round_trips`: `cw project` in a temp dir, `--days 2 --traces-per-day 300`, no payloads: `pq_cetra/` and `st/` hold the same file stems for the three tables; each Strata file's `row_count` equals its Parquet file's row count; the live part left level-`0` `spans` files; the text files parse (20 ids, 20 times, one clock).
- `payload_share_is_applied`: a `--payloads` case built with `let (spans, files) = cases::gen_spans("text_unique", Some(50))?; cases::write_case(&case_dir, spans, &files)?;`, then `--days 1 --traces-per-day 2000`: reading the project's `spans` files back, between 1% and 5% of the traces have a span with a non-null `input` (the generator's own inputs are stripped, so only pool payloads remain).
- `small_files_project`: `--small 500 --flushes 4`: four level-`0` files per table.
- `layout_counts_every_file`: `layout_report` on the first test's directory names the three tables and both formats.
- `small_form_refuses_more_than_a_day`: `--small 100000 --flushes 10` fails with the error of step 6.

### Done when

In `bench/`, with `LIBCLANG_PATH=/usr/lib/llvm-18/lib` and § Build's swap for both crates:
- `cargo build --release`, `cargo test --release cw::` pass; `cargo tree -d -e normal` shows one version each of `datafusion`, `arrow`, `parquet` and `object_store` (second versions of small crates such as `digest` or `hashbrown`, which `cetra-storage` brings, are fine).
- `cargo run --release -- cw project /tmp/cw-p --days 3 --traces-per-day 2000` and `cargo run --release -- cw layout /tmp/cw-p` print plausible tables.

**Commit:** `Add fmtbench cw project: cetra projects written by cetra-storage, converted to Strata`

---

## M3.2: many-file queries

**Goal:** the M3 queries on a project directory, cold and warm, with answers compared across formats.

**Edit:** `bench/src/main.rs` (extract `fn strata_config() -> Config` from `open_strata`: the `Config::default()` with the `FMT_OPEN_SUFFIX`, `FMT_POINT_GAP` and `FMT_POINT_MAX_RANGE` overrides; `open_strata` calls it), `bench/src/cw/mod.rs` (`pub mod many;`), `bench/src/cw/harness.rs`, `bench/src/cw/many.rs` (new). `many.rs` has its own scan code; it does not use `read.rs`'s private helpers.

### Contracts

```rust
/// A project's files of one format, per table, listed once per process from the location's
/// store (as cetra's catalog would know them, so the listing is outside every timed region).
pub(crate) struct Listing { pub spans: Vec<FileRef>, pub traces: Vec<FileRef>, pub scores: Vec<FileRef> }
/// One data file: its path, size, and day as the `YYYY-MM-DD` of its `day=…` path segment.
/// Format `pq_cetra` lists `*.parquet` under `pq_cetra/<table>/`, format `st` lists
/// `*.strata` under `st/<table>/`.
pub(crate) struct FileRef { pub path: ObjPath, pub size: u64, pub day: String }

/// Opened files of one table.
pub(crate) enum Opened { Parquet(Vec<crate::pqfast::PqFile>), Strata(Vec<Arc<StrataFile>>) }

pub(crate) async fn list(loc: &Location, format: &str) -> crate::R<Listing>;
/// Opens `files` (Parquet: `pqfast::open`; Strata: `StrataFile::open(store, path, None, &crate::strata_config())`), all at once.
pub(crate) async fn open(loc: &Location, format: &str, files: &[FileRef]) -> crate::R<Opened>;

/// `cw_traces.txt`, `cw_around.txt`, `cw_clock.txt`.
pub(crate) struct ProjectCase { pub traces: Vec<[u8; 16]>, pub around: Vec<i64>, pub clock: i64 }
pub(crate) async fn read_project_case(loc: &Location) -> crate::R<ProjectCase>;

/// The M3 queries.
pub(crate) fn is_many(query: &str) -> bool;   // open:<table>, lookup_cold, lookup_around, rollup_30d, rollup_30d_cold, anchor_rebuild
/// Cold queries open their files inside each run and take no warm-up (CW-24).
pub(crate) fn is_cold(query: &str) -> bool;   // all of them but rollup_30d

/// One run of an M3 query. `warm` holds the opened `spans` files for `rollup_30d` and is
/// `None` for cold queries. Returns the answer and, for the lookups, one latency per id.
pub(crate) async fn run(query: &str, format: &str, loc: &Location, listing: &Listing,
    warm: Option<&Opened>, case: &ProjectCase) -> crate::R<(Answer, Vec<(f64, f64)>)>;
```

| Query | What it does, on both formats | Rows | Answer bytes |
|---|---|---|---|
| `lookup_cold` | for each id of `case.traces`, in turn and timed alone (wall and CPU): open every `spans` file and decide whether it may hold the id. Parquet: the row groups whose `trace_id` min/max hold the id (footer statistics); for each, a `ParquetRecordBatchStreamBuilder::new_with_metadata(ParquetObjectReader::new(store, path), meta.clone())` and `get_row_group_column_bloom_filter(rg, leaf)` (the `trace_id` leaf index: the position in `meta.parquet_schema().columns()` whose path is `trace_id`), `Sbbf::check(&id[..])`; an absent filter may hold it; the file may hold the id if any candidate row group may. Strata: `may_contain("trace_id", id)` is not `Some(false)`. Then read the trace's rows, all columns, from the files that may hold it (Parquet: `pqfast::lookup` on the whole file; Strata: `lookup_scan`) | rows found | per id in order: its `span_id`s sorted, 8 bytes each |
| `lookup_around` | the same, but only the `spans` files whose day is within ±1 day of the id's `case.around` time (spec 03 § 6's `around` hint) | as above | as above |
| `rollup_30d`, `rollup_30d_cold` | over every `spans` file: rows with `span_kind = 'llm'`, grouped by `request_model` (a null model is the group `""`): count, the sum of `round(cost_total × 1e6)` as `i64` per row (an integer, so the order of the additions doesn't matter), and `sum(total_tokens)` (nulls add 0). Units: Parquet one per (file, row group), with a `RowFilter` on `span_kind` and the three other columns projected; Strata one per file range of `strata_ranges`-like cuts of 100,000 rows, with `Predicate::Cmp(span_kind index, CmpOp::Eq, Lit::Utf8("llm"))`, `views: false`, threads 1. Units run with `futures::stream::iter(..).buffer_unordered(cores)`; their maps are merged | groups | groups sorted by name, each: the name's bytes, `0x00`, count `u64`, cost micro-units `i64`, tokens `i64`, little-endian |
| `open:<table>` | open every file of the table and keep them all open until the run ends | files | the file count, `u64` |
| `anchor_rebuild` | spec 03 § Trace anchor: open every `traces` file, skip those whose largest `last_ingest_time` is under `clock − 2 h`, both at file level (Parquet: the largest row-group max statistic of the column; Strata: `int_range(last_ingest_time index)`), read the rest projected to `trace_id`, `start_time`, `end_time`, `last_ingest_time`, keep rows with `last_ingest_time ≥ clock − 2 h` whose `start_time` or `end_time` is within 1 h of a UTC midnight | entries | the `trace_id`s sorted, 16 bytes each |

- cetra reads `traces` files by its catalog's `max_ingest_time` without opening them; both formats open them here, the same way, so the comparison holds.
- `PqFile`'s `file_size`, `trace_root` and `trace_leaf` are private: build readers with `ParquetObjectReader::new(f.store.clone(), f.path.clone())` (no file size; the metadata is already loaded) and compute the leaf index as above.
- M3 queries report `waves` as `null` (their files are opened inside the run or by `many`, not through `read::Files`).
- On R2, `lookup_cold` reopens every `spans` file for each of 20 ids: about 12,000 opens a run for 600 files. M3.3 uses fewer rounds there (below).

### Steps

1. `main.rs`: extract `strata_config()`; `open_strata` behaves as before.
2. `read.rs`: the `pub(crate)` changes only.
3. `many.rs`: the functions above.
4. `harness.rs`, in `one`: a query with `many::is_many` goes to a new `many_format(dir, format, query, runs)`, before any write-like fall-through. It lists the files (`many::list`) and reads the case outside the timed region. Warm (`rollup_30d`): open the `spans` files once, one warm-up run, then `runs` measured runs, as `read_format` does. Cold: no warm-up; before each run, reset the store counters (`loc.store.reset()`), so `gets` and `bytes` are one cold run's. For the lookups, `runs_ms` and `cpu_ms` get one entry per id (20 per run), so the summary's p95 and p99 are per lookup; for the others, one entry per run. Every run's answer must equal the first run's.

### Tests (`cw/many.rs`)

On a `cw project --days 3 --traces-per-day 400` directory, made once per test in a temp dir:
- `lookups_agree_across_formats`: `lookup_cold` and `lookup_around` give equal answers on both formats and find every id; `lookup_around` makes fewer GETs than `lookup_cold` on each format (`loc.store.stats()`).
- `rollup_agrees_across_formats`: equal answers, warm and cold.
- `anchor_rebuild_agrees`: equal answers on both formats, at least one entry (the live part crosses a midnight), and equal to a reference computed from every `traces` row read in full with `ParquetRecordBatchReaderBuilder`.
- `rollup_sums_are_order_free`: the same rollup over the files listed in reverse order gives the same answer.
- `open_counts_every_file`: each `open:<table>` answer equals the number of that table's files in the listing.

### Done when

- `cargo build --release`, `cargo test --release cw::` pass.
- `cargo run --release -- cw project /tmp/cw-p3 --days 3 --traces-per-day 2000`, then `cargo run --release -- cw run /tmp/cw-p3 --rounds 2 --runs 2` finish with no `ANSWER MISMATCH` and a verdict per query.

**Commit:** `Add fmtbench cw many-file queries: cold lookups, rollups, opens, anchor rebuild`

---

## M3.3: runs and report (orchestrator)

On ovh-us, idle, after M2's runs:
1. Sync `crates/storage` to ovh-us and add the three dependencies to its `bench/Cargo.toml` with its own paths.
2. The small-files project: `cw project ~/cw/small --small 2000 --flushes 100`, then `cw layout` and `cw run … --rounds 26 --runs 2`; small `spans` write time: `cw gen text_unique ~/cw/small-w --traces 4000`, `cw split ~/cw/small-w 100`, `cw run ~/cw/small-w --queries write --rounds 26 --runs 2`.
3. The small 30-day project: `cw project ~/cw/p30-small --days 30 --traces-per-day 50000 --payloads ~/cw/swe,~/cw/wildchat` (1.5M traces); `cw layout`; `cw run … --rounds 26 --runs 2` locally; upload `pq_cetra st` and the text files to `s3://cetra-fmtbench-us/cw/p30-small` and run there with the S3 profile (`FMT_POINT_GAP=262144 FMT_POINT_MAX_RANGE=1048576`), with `--rounds 6 --runs 1` for `lookup_cold` (each run is 20 cold lookups over every file) and `--rounds 26 --runs 2` for the rest.
4. The verdict project, `--traces-per-day 1000000`, after timing step 3's ingestion and checking free disk; the same runs.
5. Write M3's verdicts in `strata-readiness.md` § M3, with the `layout` tables, and mark M3 in the design doc.

## Unverified

- How many files cetra's compaction leaves per day at 1M traces a day: `cw layout` reports it; the design's "100–500 files" is a guess.
- Ingestion speed through `cetra-storage` at 30M traces on ovh-us: M3.3 times the small project first.
