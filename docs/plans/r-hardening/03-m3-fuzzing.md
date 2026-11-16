# R · M3 — Fuzzing

The third milestone of track R (`00-design.md` § M3, decisions D-R-7 and D-R-14). Coverage-guided fuzzing with cargo-fuzz and libFuzzer runs every decoder that reads bytes we did not just write: WAL records and segments, Parquet files, read-cache blocks, `catalog.db` rows, the filter DSL, and OTLP requests. A replay test per target runs the committed corpus on stable in `just test`. A nightly workflow fuzzes each target for 10 minutes. It also runs the long model test (D-R-14), M6's stress runs (30 minutes local, 10 minutes remote), and M2's LazyFS check when GitHub's runners allow it. It does not run M5's disk-full tests.

**Why it matters.** Goal 3 of the track is "never panics or wedges on bad bytes". Today nothing fuzzes our decoders, and malformed Parquet can still panic `parquet`/`arrow` (arrow-rs#8602, #9868). Reading the code for this plan found that **a decoder panic escapes every Parquet read path today** (§ R3.3 *Today*). The milestone therefore also adds an error boundary at our Parquet reads, so that a panic becomes a decode error and goes through quarantine (D-A1-10).

Sub-phases R3.1–R3.8, one commit each. R3.8 can produce several commits, one per bug found.

## Conventions for every sub-phase

- **`just check` passes before each commit.** Node comes from nvm and is not on the non-interactive PATH: run `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` first (if that version is gone, `ls ~/.local/share/nvm/` for the current one). One commit per sub-phase, with the subject given.
- **M3 starts after M2 is merged.** M2's `src/fs.rs` seam, lossy crashes and fsync fixes are in place. The signatures this plan names (`recover_table`, `ReadCache::open`, `Disk::open`, `SegmentWriter::create`) are those read before M2. Where M2 changed one (an added `fs` handle, for example), call M2's final signature with the production, non-lossy value: under `cfg(fuzzing)` there is no lossy mode. Say in the report what you passed. The fuzz entries write their own input files with plain `std::fs`: they are harness code, not storage paths.
- **Scope.** Edit only:
  - `crates/storage/src/fuzz_entry.rs` (new), `crates/storage/src/unwind.rs` (new), `crates/storage/src/lib.rs` (the `mod` lines and the small refactors named below), and the storage files each sub-phase names (including `catalog/mod.rs` and `read_cache.rs` in R3.4, and the tests R3.3 names);
  - `crates/storage/Cargo.toml` (R3.4), `crates/otel/src/fuzz_entry.rs` (new) and `crates/otel/src/lib.rs` (R3.5);
  - the root `Cargo.toml`: the `unexpected_cfgs` lint (R3.1) and the `arbitrary` pin (R3.4) only;
  - `fuzz/` (new), `justfile`, `deny.toml`, `.github/dependabot.yml`, `.github/workflows/nightly.yml` (new), `AGENTS.md` (R3.6, R3.7), `CHANGELOG.md` (R3.3);
  - `docs/`.

  R3.8 may fix bugs anywhere in `crates/storage/src/` and `crates/otel/src/` (D-R-2). It stops and reports instead when a fix would change a file format, a public API or a spec rule.
- **Dependencies** (versions looked up on 2026-11-14 with `cargo info`):
  - `arbitrary = { version = "1.5.0", features = ["derive"] }` in the root `[workspace.dependencies]` (MIT OR Apache-2.0; brings `derive_arbitrary` 1.5.0). `cetra-storage` uses it as a `cfg(fuzzing)` target dependency and a dev-dependency (R3.4).
  - `libfuzzer-sys = "=0.4.13"` (an exact pin) in `fuzz/Cargo.toml` only (`(MIT OR Apache-2.0) AND NCSA`). It is outside the workspace, so the "versions once in the root" rule does not apply to it (D-R-7). Its dependencies (`arbitrary` 1.x, and `cc` with `jobserver`, `shlex` and `find-msvc-tools` at build time) are already in the root `Cargo.lock` once R3.4 adds `arbitrary`.
  - `cargo-fuzz` 0.13.2, installed by `just fuzz-tools`, not a dependency.
  - The fuzzing toolchain is `nightly-2026-11-03`, a `justfile` variable (`fuzz_toolchain`). Updated by hand, like `rust-toolchain.toml`. If that date does not install (`rustup toolchain install nightly-2026-11-03 --profile minimal`), take the newest date that does and say which in the report.
- **Entry points** (D-R-7). Each crate has one module `fuzz_entry`, declared in its `lib.rs` as:

  ```rust
  /// Fuzz entry points (track R M3). Not part of the API.
  #[cfg(any(test, fuzzing))]
  #[doc(hidden)]
  pub mod fuzz_entry;
  ```

  The first line of each `fuzz_entry.rs`, after the module doc, is the lint allowance `crates/eval/src/trace.rs` uses. `expect_used` and `unwrap_used` are workspace lints and clippy runs with `-D warnings`; `clippy.toml`'s allow-in-tests covers `cfg(test)` only, not `cfg(fuzzing)`:

  ```rust
  #![allow(clippy::expect_used, clippy::unwrap_used)] // harness setup, not input-dependent
  ```

  Each target is one `pub fn <target>(data: &[u8])`. It returns nothing and must never panic on any input: errors are fine and are dropped. It may panic only:
  - on a broken harness environment (a runtime or a temp directory that cannot be built), with an `expect` whose message says so and a `// Harness setup, not input-dependent.` comment, as AGENTS.md allows;
  - in the assertions this plan names explicitly. Those are properties; a failing one is a finding (D-R-2).
- **`cfg(fuzzing)` and stable.** cargo-fuzz builds with `--cfg fuzzing` in `RUSTFLAGS`, for every crate in the graph. Stable `rustc` warns about an unknown cfg (`unexpected_cfgs`), and clippy runs with `-D warnings`, so R3.1 declares it in the root `[workspace.lints.rust]`. Under `just test` the entries compile as `cfg(test)` and clippy checks them (`--all-targets`).
- **Harness runtime.** Async entries share one runtime per crate:

  ```rust
  fn runtime() -> &'static tokio::runtime::Runtime {
      static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
      RUNTIME.get_or_init(|| {
          tokio::runtime::Builder::new_multi_thread()
              .worker_threads(2)
              .enable_all()
              .build()
              // Harness setup, not input-dependent.
              .expect("build the fuzz runtime")
      })
  }
  ```

  Replay tests call the entries from many test threads at once; `block_on` on a multi-thread runtime allows that. `runtime()` is `pub(crate)` so R3.3's tests can use it.
- **No dead code between sub-phases.** clippy runs with `-D warnings`, so each sub-phase adds only the helpers it uses:
  - storage: R3.1 adds `Cursor` with `u8`, `u32`, `u64` and `bytes`; R3.2 adds `i64`, `rest` and `runtime()`;
  - otel: R3.5 adds its own `runtime()` only if an otel entry needs one (they are synchronous, so normally not), and no `Cursor`.

  The same holds for every helper in this plan: it lands with its first caller.
- **Scratch directories.** An entry that needs files creates `std::env::temp_dir().join(format!("cetra-fuzz-{}", Uuid::now_v7()))` and removes it with `std::fs::remove_dir_all` before returning, on every path (a small guard struct with `Drop` is fine). The nightly sets `TMPDIR=/dev/shm` so this is tmpfs.
- **Replay tests.** Each crate's `fuzz_entry.rs` ends with `#[cfg(test)] pub(crate) mod tests` (`pub(crate)` so R3.3's tests in `unwind.rs` can call its `pub(crate)` seed builders), holding one test per target and this helper:

  ```rust
  /// Runs `entry` on every file of `fuzz/corpus/<target>/` and
  /// `fuzz/regressions/<target>/`. The corpus must not be empty.
  fn replay(target: &str, entry: fn(&[u8]));
  ```

  It builds the paths from `env!("CARGO_MANIFEST_DIR")` + `../../fuzz`. A missing `regressions/<target>` directory counts as empty; a missing or empty `corpus/<target>` fails with `"no corpus for {target}"`. Files are read in sorted order, and a panic names the file (`std::panic::catch_unwind` around the call, then `panic!("{target}: {path} panicked")`). Each replay test must run in under 10 s in debug; R3.8 measures it.
- **Seeds.** Each crate's tests also hold `#[test] #[ignore] fn write_seed_corpus()`, which writes the seed files of that crate's targets into `fuzz/corpus/<target>/seed-<name>`. `just fuzz-seed` (R3.1) runs both crates' versions: its `--exact` filter matches the same path in each.
  - Every seed comes from a **seed builder**, a `fn seed_<what>(…) -> Vec<u8>` in `fuzz_entry::tests` that returns the whole entry input (header included). Tests that need a real input call the builder, never a file in `fuzz/corpus/`: `cargo fuzz cmin` renames corpus files to content hashes, so `seed-*` names don't survive it.
  - The output is deterministic. Before the first `cmin`, rerunning `fuzz-seed` leaves the tree clean. After a `cmin`, it adds the `seed-*` files back; follow it with `just fuzz-cmin`, which removes the duplicates again.
- **Logging and privacy.** The entries log nothing. New production code (R3.3) logs static messages only: no payload, no panic message (a panic message can quote a value), no path.
- **Panics in tests.** Test code may `unwrap`/`expect` (`clippy.toml` allows it).
- **Report at the end of each sub-phase:**
  - what was done;
  - any deviation from this plan, and why;
  - how long each new replay test took in debug (`cargo test -p <crate> --all-features fuzz_entry`);
  - anything left undone or unverified.

### Targets and input layouts

| Target | Crate | Input | Entry calls |
|---|---|---|---|
| `wal_record` | storage | raw bytes | `decode_next` over the bytes; `read_batch` of the bytes as a payload, both tables; `replay_batch` into a memtable |
| `wal_replay` | storage | header + segments of CRC-framed or raw bytes | `recover_table` on a scratch WAL directory, twice |
| `parquet_file` | storage | header + file | the read API through `project_query`, `verify_file`, the anchor rebuild, `slice::time_values` |
| `read_cache_block` | storage | `arbitrary` | `ReadCache::open` over a fuzzed cache directory, then `CacheStore` reads checked against the true bytes |
| `catalog_rows` | storage | `arbitrary` | the four loads `Committer::open` and `Storage::open` run, then `compaction::plan::plan` |
| `filter_dsl` | storage | view byte + UTF-8 | `QueryContext::parse_filter`, then apply and collect, on a 2 MiB stack |
| `otlp_http_protobuf` | otel | raw bytes | `decode_protobuf`, `normalize`, `spans_to_record_batch` |
| `otlp_http_json` | otel | raw bytes | `decode_json`, `normalize`, `spans_to_record_batch` |
| `otlp_grpc` | otel | raw bytes | `decode_protobuf`, `encoded_len` |

`otlp_grpc` is decode only (`00-design.md` § M3): the server's gRPC path decodes with tonic's prost codec into the same `ExportTraceServiceRequest` (`crates/server/src/ingest/grpc.rs`), sizes it with `encoded_len`, then normalizes exactly as HTTP does. Without `normalize`, the fuzzer runs prost's decoder many times faster than `otlp_http_protobuf` can. gRPC framing and gzip are tonic's code, outside our crates.

Storage's headers are read with a small private cursor in its `fuzz_entry.rs` (methods added as § Conventions says):

```rust
struct Cursor<'a> { data: &'a [u8] }
impl<'a> Cursor<'a> {
    fn u8(&mut self) -> u8;            // 0 when empty
    fn u32(&mut self) -> u32;          // little-endian; missing bytes read as 0
    fn u64(&mut self) -> u64;
    fn i64(&mut self) -> i64;
    fn bytes(&mut self, n: usize) -> &'a [u8]; // at most n, fewer when short
    fn rest(self) -> &'a [u8];
}
```

The seed writers build headers with the matching little-endian encoding.

---

## R3.1 — Entry module, the WAL targets, replay and seeds

Commit: `storage: add the WAL fuzz entry points and their corpus replay (R3.1)`

### Root `Cargo.toml`

Add to `[workspace.lints.rust]`:

```toml
# cargo-fuzz builds with `--cfg fuzzing` (track R M3, D-R-7).
unexpected_cfgs = { level = "warn", check-cfg = ["cfg(fuzzing)"] }
```

### Small refactors

- **`wal/mod.rs`.** `record` is a private module. Add:

  ```rust
  #[cfg(any(test, fuzzing))]
  pub(crate) use record::{Decoded, decode_next, encode, push_framed, read_batch};
  ```

  `HEADER_LEN` is used only by the `#[cfg(test)]` seed builder: import it there as `crate::wal::record::HEADER_LEN` behind a `#[cfg(test)] pub(crate) use` in `wal/mod.rs`, not in the `cfg(fuzzing)` list (a fuzz build would warn it unused).
- **`wal/record.rs`, `push_framed`.** Change its `#[cfg(test)]` to `#[cfg(any(test, fuzzing))]`, and replace its `u32::try_from(payload.len()).unwrap()` with `u32::try_from(payload.len()).unwrap_or(u32::MAX)` (the fuzz entry caps payloads far below that; a capped length then fails the length check on decode, it never panics).
- **`lib.rs`, `recover_table`.** Move the body of its `on_batch` closure into a function, and call it from the closure:

  ```rust
  /// Inserts one replayed batch into `memtable`, as WAL recovery does. An empty batch is skipped.
  fn replay_batch(memtable: &mut Memtable, seq: u64, batch: RecordBatch) -> Result<(), WalError>;
  ```

  The closure becomes `&mut |seq, batch| replay_batch(&mut memtable, seq, batch)`. No behaviour change.
- **`lib.rs`.** Add the `fuzz_entry` module declaration of § Conventions. `fuzz_entry` is a child of the crate root, so it can call the root's private functions (`recover_table`, `replay_batch`, `project_query`).

### `crates/storage/src/fuzz_entry.rs`

Module doc: `//! Fuzz entry points (track R M3, docs/plans/r-hardening/03-m3-fuzzing.md). Each one takes arbitrary bytes and must not panic.`

**`pub fn wal_record(data: &[u8])`:**
1. Framing: `let mut offset = 0;` loop on `decode_next(data, offset)`: on `Record { next, .. }`, assert `next > offset` (property: framing always advances) and set `offset = next`; stop on `End`, `Short` or `Invalid`.
2. For `table` in `[Table::Spans, Table::Scores]`: `read_batch(data, table)`. On `Ok(batch)`:
   - `replay_batch(&mut Memtable::new(table), 1, batch.clone())`; the result is dropped.
   - Round trip (property): `encode(1, &batch, &mut out)` is `Ok`; `decode_next(&out, 0)` is a `Record { payload, .. }`; `read_batch(&out[payload], table)` is `Ok` with the same `num_rows()` and the same `schema().fields()`. Do not compare batches with `==`: float columns may hold NaN.

**`pub fn wal_replay(data: &[u8])`.** Raw bytes would almost never pass a record's CRC-32C (`decode_next`), so replay would never reach `read_batch` or the memtable. The entry therefore frames most segments itself with the real checksum, and keeps a raw mode for the framing code. Layout:
- byte 0: bit 0 = table (`0` spans, `1` scores), bit 1 = `skip_corrupt`;
- byte 1: segment count `1 + byte % 3`;
- bytes 2..10: `after_seq` (u64);
- each segment:
  - 8 bytes `first_seq` (u64), then 1 byte `mode`;
  - `mode` bit 0 = 0, **framed**: 1 byte frame count `n % 5`, then per frame 8 bytes `seq` (u64), 4 bytes `len` (u32, clamped to what is left and to 1 MiB) and `len` payload bytes; the entry builds the segment with `push_framed(&mut segment, seq, payload)` per frame. Then `mode >> 1` is a tear: cut `(mode >> 1) % 32` bytes off the segment's end (saturating);
  - `mode` bit 0 = 1, **raw**: 4 bytes `len` (u32, clamped) and `len` bytes, written as they are.

Steps:
1. Create the scratch directory and `dir = scratch.join(table.name())`; write each segment to `dir.join(segment_name(first_seq))` (a repeated name overwrites, which is fine).
2. `recover_table(&dir, table, after_seq, skip_corrupt, WalConfig::default().segment_bytes.0, &Hooks::default())`. On `Err`, return.
3. On `Ok(first)`: keep `first.records` and `first.table.memtable.bytes()`, drop `first` (it holds the new segment's file), and call `recover_table` again with the same arguments. **Property:** the second call is `Ok`, with the same `records` and the same memtable bytes. A replay that truncated a torn tail and created the next segment must replay the same way on the next restart.

### Tests (`fuzz_entry::tests`)

- `wal_record_corpus_replays`: `replay("wal_record", wal_record)`.
- `wal_replay_corpus_replays`: `replay("wal_replay", wal_replay)`.
- Seed builders (§ Conventions):
  - `fn ipc_payload(table: Table) -> Vec<u8>`: the payload of `encode(1, &batch, &mut out)` (`out[HEADER_LEN..]`) for `wal::sample_span_batch()` (spans) or `test_support::score_rows(&test_support::dataset_scores())` (scores);
  - `fn seed_wal_replay(table: Table, skip_corrupt: bool, segments: &[(u64, &[u64], u8)]) -> Vec<u8>`: one framed segment per `(first_seq, record seqs, tear)`, each frame carrying `ipc_payload(table)`.
- `wal_replay_replays_framed_records`: `seed_wal_replay(Table::Spans, false, &[(1, &[1, 2, 3], 0)])`, run the entry's steps by hand, and check the first `recover_table` returns `records == 3`. This proves the layout reaches real replay and the memtable.
- `wal_replay_truncates_a_torn_frame`: the same with a tear of 5: `records == 2`, and the second replay agrees.
- `write_seed_corpus` (`#[ignore]`):
  - `wal_record/seed-spans`, `wal_record/seed-scores`: `ipc_payload(table)`.
  - `wal_replay/seed-one-segment`: `seed_wal_replay(Spans, false, &[(1, &[1, 2, 3], 0)])`.
  - `wal_replay/seed-torn-tail`: the same with a tear of 5.
  - `wal_replay/seed-two-segments`: `seed_wal_replay(Scores, true, &[(1, &[1, 2], 0), (3, &[3], 0)])`.

### `justfile`

Add after `test:`:

```just
# Write the fuzz seed corpus from the test fixtures (track R M3)
fuzz-seed:
    cargo test --workspace --all-features -- --ignored --exact fuzz_entry::tests::write_seed_corpus
```

### Commands

```bash
just fuzz-seed
cargo test -p cetra-storage --all-features fuzz_entry
git status --short fuzz/
```

Expected: 4 tests pass; the five seed files are new and are committed with this sub-phase. Running `just fuzz-seed` again leaves `git status` unchanged (no `cmin` has run yet).

---

## R3.2 — `parquet_file` and `filter_dsl`

Commit: `storage: add the Parquet and filter fuzz entry points (R3.2)`

### Small refactor

**`anchor.rs`, `Anchors::rebuild_file`.** Make it `pub(crate)` and take `store: Arc<dyn ObjectStore>` instead of `objects: &Objects`. `rebuild` passes `objects.store()`. No behaviour change. (`compaction/slice.rs::time_values` is already `pub(crate)` and takes a store.)

### `pub fn parquet_file(data: &[u8])`

Layout: byte 0 = table (`% 3`: `FileTable::Spans`, `Traces`, `Scores`), bytes 1..5 = footer hint (u32), bytes 5..13 = `raw_bound` (i64), rest = the file. An empty file returns.

**Day and bound.** `SEED_DAY` is the UTC day of the seed data: read the `start_time` of `test_support::dataset_spans()` once while implementing (both spans and scores of the dataset should fall on it; if they don't, stop and report) and write it as a `const` date. `meta.day = SEED_DAY`, and `bound = midnight(SEED_DAY) + raw_bound.rem_euclid(DAY_NS)`. The windowed reads below use `[bound, bound + 1 day)`, which always overlaps `SEED_DAY`: day pruning (widened by a day for spans and traces, exact for scores, `query/mod.rs::file_day_range`) never skips the file, so the decoder always runs.

Build the read state in `pub(crate) fn parquet_query(store: Arc<dyn ObjectStore>, meta: &FileMeta) -> (QueryContext, Arc<VerifyQueue>)` (R3.3's tests reuse it; `FileId`'s inner `Uuid` is `pub(crate)`, so `FileId(Uuid::from_u128(1))` builds from here):
1. The caller has put the file into `store` at `meta.path` = `tables/<table>/fuzz.parquet` (an `InMemory` in the entry).
2. `meta = FileMeta`:
   - `id: FileId(Uuid::from_u128(1))`, `table`, `project: PROJECT` (`"018f6b4e-7c1a-7c1a-8c1a-000000000001"`, the project of `test_support::span_row`, so seed rows are not filtered out), `day: SEED_DAY`, `level: 0`;
   - `path`, `size: file.len()`, `footer_size: hint`, `rows: 0`;
   - `min_time`, `min_ingest_time`: `i64::MIN`; `max_time`, `max_ingest_time`: `i64::MAX`, so no pruning skips the file;
   - `fully_merged: false`, `deletions_applied_seq: 0`.
3. `catalog = Catalog::empty().apply(&CatalogEdit { add: vec![meta.clone()], remove: Vec::new(), flushed_seq: None, sources: Vec::new(), raise: Vec::new(), quarantine: Vec::new() })`, which returns `Result<(Catalog, Vec<Arc<FileMeta>>), _>`: take `.expect("one new file applies to an empty catalog").0`, as `query/mod.rs::tests::catalog_with` does with `unwrap().0`. If `CatalogEdit` has more fields, the compiler says so: give them their empty value and report it.
4. `state = ReadState { catalog: Arc::new(catalog), ..ReadState::empty() }`.
5. `runtime_env = RuntimeEnvBuilder::new().build_arc()`; register `store` under `OBJECT_STORE_URL`, parsing the URL as `objects.rs` does (`RegisteredUrl::parse(OBJECT_STORE_URL)`, near line 587; copy that call).
6. `verify = Arc::new(VerifyQueue::default())`; `project_query(&state, PROJECT, QueryOptions::default(), &runtime_env, TWO, lookback, &verify)` with `lookback` = one day in ns and `const TWO: NonZeroUsize = NonZeroUsize::new(2).expect("2 is not zero");` (`NonZeroUsize::new` returns an `Option`; the module's lint allowance covers the `expect`). Then `QueryContext::new(query, None)` (harness setup: `expect`).

Then, inside `runtime().block_on`, every result dropped (`timeout: Duration::from_secs(30)` everywhere):
- **Spans:** `span_list` with no filter, no search, `scope` with no window and both `include_*` true, `SpanSort::StartTime`, `SortOrder::Desc`, `limit: 100`, no cursor; again with `filter: Some("status = 'error'")` and `scope.window = TimeWindow::new(bound, bound.saturating_add(DAY_NS)).ok()`, so the predicate and the page index are used.
- **Traces:** `trace_list` with the same two variants (`TraceSort::StartTime`), and `trace_detail(&ctx, "00010001000100010001000100010001", timeout)`.
- **Scores:** `score_list` with every filter `None`, `window: None`, `limit: 100`.
- **Every table:** `verify_file(&store, &meta)`.
- **Traces:** `Anchors::new(DAY_NS).rebuild_file(&meta, Arc::clone(&store), bound)`. `rebuild_file` takes `&mut self`; calling it on the temporary is fine.
- **Spans and scores:** `compaction::slice::time_values(Arc::clone(&store), &[Arc::new(meta.clone())], column)` with `spans::START_TIME` / `scores::CREATED_AT`.

### `pub fn filter_dsl(data: &[u8])`

Byte 0 = view (`% 4`: `FilterView::Traces`, `Spans`, `Sessions`, `Scores`); the rest must be UTF-8 (`std::str::from_utf8`), else return.

Run on a thread with a 2 MiB stack, the size of a tokio worker's stack, where the server parses filters (`filter/mod.rs` has a test on the same stack size): `std::thread::Builder::new().stack_size(2 * 1024 * 1024).spawn(...)`, then `join()`. A join error means a panic inside: `std::panic::resume_unwind(payload)`, so the fuzzer and the replay test see it. A stack overflow aborts the process; the fuzzer reports it as a crash, which is what we want.

On that thread, inside `runtime().block_on`:
1. Build a `ProjectQuery` over empty tables: `MemTable::try_new(schema, vec![vec![]])` for `cetra_core::schema::{spans_schema, traces_schema, scores_schema}()`, `runtime: Arc::new(RuntimeEnv::default())`, `target_partitions: TWO`, `optimizer_rules: Vec::new()`, `suspects: Suspects::new(Arc::new(VerifyQueue::default()))`. Then `QueryContext::new(query, None)` (harness setup: `expect`).
2. `ctx.parse_filter(view, text)`. On `Ok(filter)`: `ctx.session().table(name)` with the view's `ViewName` name (map the four `FilterView`s to `ViewName`s in the entry; `FilterView::as_str` is private to `filter`), then `filter.apply(frame, &ctx)`, then `.collect()`. Drop every result.

### Tests

- `parquet_file_corpus_replays`, `filter_dsl_corpus_replays`.
- Seed builders: `fn parquet_bytes(table: FileTable) -> WrittenFile`, which is `parquet::write_file(table, 0, &[Arc::new(batch)])` of `span_rows(&dataset_spans())`, `trace_batch(&dataset_spans())` or `score_rows(&dataset_scores())`; and `fn seed_parquet(table: FileTable) -> Vec<u8>`, which is the entry input: the table byte, `written.footer_size` as u32, `raw_bound = 0`, then `written.bytes`. Both are `pub(crate)` inside `#[cfg(test)]` so R3.3's tests use them.
- `parquet_file_reads_a_seed`: with `parquet_bytes(FileTable::Spans)`, `parquet_query` then `span_list` returns `Ok` with as many rows as `dataset_spans()`. Proves the project and day reach real rows.
- `write_seed_corpus` gains `parquet_file/seed-spans`, `seed-traces`, `seed-scores`: `seed_parquet(table)`.
  - `filter_dsl/seed-<n>`: one file per line below, view byte first. From `filter/mod.rs`'s tests: `status = 'error'` (spans); `cost_total > 0.001 AND tags @> ['beta']` (spans); `start_time >= '2026-01-01T10:05:00Z'` (traces); `start_time > now() - INTERVAL '3650 days'` (traces); `trace_count >= 2` (sessions); `name = 'helpfulness' AND value_num > 0.5` (scores); `environment = 'production' OR status = 'ok'` (traces); `name = 'a' extra` (spans, an error case).

### Commands

```bash
just fuzz-seed
cargo test -p cetra-storage --all-features fuzz_entry
```

Expected: the new tests pass; commit the new seed files.

---

## R3.3 — Decoder panics become decode errors

Commit: `storage: turn Parquet and Arrow decoder panics into decode errors (R3.3)`

### Today

Read from the code (DataFusion 55.2.0, parquet and arrow 59.3.2 per `Cargo.lock`), confirmed by this sub-phase's tests:
- **Queries.** The Parquet scan (`DataSourceExec` over `ParquetSource`) decodes inside its stream's `poll_next`; neither it nor the Parquet opener spawns tasks (`datafusion-datasource-parquet` spawns only in its writer). When a `RepartitionExec`/`CoalescePartitionsExec` polls the scan on a spawned task, DataFusion re-raises the panic in the consumer (`resume_unwind` in `datafusion-common-runtime/src/common.rs` and `datafusion-physical-plan/src/stream.rs`). The panic reaches the axum handler's task. Tokio catches it at the task boundary (no profile sets `panic = "abort"`), so the process survives, but the client's connection is dropped and **`Suspects::report` never runs**. The file is never verified or quarantined, and every later query that reads it fails the same way.
- **Compaction and rewrite jobs** run in a `JoinSet` (`compaction/job.rs`, `Compactor::pass`): the panic becomes a join error and the job counts as failed. `Compactor::run`'s suspect step is skipped by the unwind, so the job fails again on every pass.
- **Verification** (`verify_file`) and **the media GC** run inside the compaction task's pass, not in the `JoinSet`. A panic there ends the **background compaction task**: compaction, deletion passes and verification stop until a restart.
- **Open.** The anchor rebuild (`anchor.rs`, `Anchors::rebuild`) reads recent `traces` files inside `Storage::open`. A panic there makes `open` panic: the server cannot start.
- **WAL replay** runs in `spawn_blocking`; a panic in Arrow's IPC reader becomes `OpenError` "replay task panicked". The server does not start, and `wal.skip_corrupt` cannot help, because no error was returned.

Catching the panic is possible at each of those boundaries: the decode runs in a `poll` or a call of our own code (`std::panic::catch_unwind`, `futures::FutureExt::catch_unwind`). The state a panic leaves behind belongs to the reader, which is dropped. DataFusion's metadata cache takes its mutex only around map operations, not around decoding (`datafusion-execution-55.2.0/src/cache/default_cache.rs`); test 3 below checks that the runtime still works after a caught panic.

**In fuzz builds**, libfuzzer-sys installs a panic hook that aborts *before* unwinding (`libfuzzer-sys` 0.4.13 `src/lib.rs`, `initialize`). A guarded panic is still reported as a crash by the fuzzer, which is what we want: it is an upstream bug to report (user decision: known upstream panics stay crashes). The stable replay tests do unwind, so they check that **no panic escapes the guard**. That is why `fuzz/regressions/` is replayed only by the stable tests and never passed to libFuzzer (R3.6).

**Residual.** The default panic hook still prints the panic message to stderr in production, before the guard catches the unwind. An upstream message could quote a value. Our code logs no panic message; a server panic hook is out of scope (§ What not to build) and is listed in `hardening.md`.

### `crates/storage/src/unwind.rs` (new)

```rust
//! The error boundary around Parquet and Arrow decoding (spec 03 §10): a
//! decoder panic becomes a decode error, so the read fails, its files become
//! suspects, and verification can quarantine them.

/// The error a caught decoder panic becomes. `verify::may_be_corrupt` treats
/// it as a decode error.
pub(crate) fn decoder_panicked() -> ParquetError {
    ParquetError::General("decoder panicked".to_owned())
}

/// Runs `f`, turning a panic into `Err(decoder_panicked())`.
pub(crate) fn guard_sync<T>(f: impl FnOnce() -> Result<T, ParquetError>) -> Result<T, ParquetError>;

/// Polls `future`, turning a panic into `Err(decoder_panicked())`.
pub(crate) async fn guard<T>(future: impl Future<Output = Result<T, ParquetError>>) -> Result<T, ParquetError>;

/// A stream whose panics become one `on_panic()` item, after which it ends.
pub(crate) struct GuardedStream<S: Stream> {
    inner: Option<Pin<Box<S>>>,
    on_panic: fn() -> S::Item,
}

/// Wraps a Parquet scan: its stream's panics become a
/// `DataFusionError::ParquetError(decoder_panicked())`.
#[derive(Debug)]
pub(crate) struct GuardExec { input: Arc<dyn ExecutionPlan> }

/// `DataSourceExec::from_data_source(config)` inside a `GuardExec`. Every
/// Parquet scan is built with this.
pub(crate) fn guarded_scan(config: FileScanConfig) -> Arc<dyn ExecutionPlan>;
```

`GuardedStream` has two users (the Parquet streams below, with `ParquetError`, and `GuardExec`, with `DataFusionError`), which is why it is generic.

- `guard_sync`: `std::panic::catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| Err(decoder_panicked()))`.
- `guard`: `AssertUnwindSafe(future).catch_unwind().await` (`futures::FutureExt`), same mapping.
- `GuardedStream::poll_next`: take `inner` (return `Ready(None)` when it is `None`); `catch_unwind(AssertUnwindSafe(|| inner.as_mut().poll_next(cx)))`. On `Ok(poll)`: put `inner` back and return `poll`. On a panic: leave `inner` as `None` (the reader is dropped and never polled again), log `tracing::error!("a Parquet decoder panicked; the read fails as a decode error")`, and return `Ready(Some((self.on_panic)()))`.
- `GuardExec` implements `ExecutionPlan` and `DisplayAs` as a **transparent wrapper**, modelled on DataFusion 55's own wrapper `CooperativeExec` (`datafusion-physical-plan-55.2.0/src/coop.rs`). Override the same trait methods it overrides and delegate to `input` the same way (input order, children, `with_new_children`, statistics, limit pushdown, cardinality, projection swap, filter pushdown, sort pushdown), except `try_to_proto`. Write it yourself from the trait docs; don't paste. `name()` is `"GuardExec"`. The wrapper is present while the physical optimizer runs (`EnforceDistribution`, `EnsureCooperative`, …), so it also needs these:
  - `properties()` returns the input's `PlanProperties` unchanged (the same `Arc`). That carries the input's partitioning, ordering, `EvaluationType` and `SchedulingType`, so `EnsureCooperative` sees the scan's own scheduling type and adds no second wrapper.
  - `benefits_from_input_partitioning()` → `vec![false]`, so `EnforceDistribution` inserts no repartition between the wrapper and the scan.
  - `repartitioned(target, config)`: `self.input.repartitioned(target, config)?` and wrap a `Some(plan)` in a new `GuardExec`. The scan's own file-group repartitioning then still works.
  - `fetch()` → `self.input.fetch()`; `with_fetch(limit)` → `self.input.with_fetch(limit)` wrapped in a new `GuardExec`. `supports_limit_pushdown()` stays `true`, as `CooperativeExec`'s does, so `LimitPushdown` pushes a limit through the wrapper into the scan.
  - `with_preserve_order(preserve)` and `with_new_state(state)`: forward to `input` and wrap a `Some` result in a new `GuardExec` (both are trait methods with a `None` default; forwarding keeps order-preserving and dynamic-state rewrites reaching the scan).
  - `metrics()` → `self.input.metrics()`, so `EXPLAIN ANALYZE` (the M3 benchmark of A2) still shows the scan's metrics.

  `execute(partition, context)`:
  - `catch_unwind` around `self.input.execute(partition, context)`, a panic mapping to `Err(DataFusionError::ParquetError(Box::new(decoder_panicked())))`;
  - `Ok(Box::pin(RecordBatchStreamAdapter::new(schema, GuardedStream::new(stream, || Err(DataFusionError::ParquetError(Box::new(decoder_panicked())))))))`.

  Wrapping at construction keeps every optimizer rule working through a transparent node. It also covers the plans the trace list fast path builds and runs at execution time (`query/traces.rs`, `run_list` → `build_range_plan` → `collect_plan`), which no optimizer rule visits; an appended physical optimizer rule would miss those.

### Every Parquet read goes through the guard

| Site | Change |
|---|---|
| `query/mod.rs`, `file_plan` | `Ok(guarded_scan(builder.build()))` instead of `DataSourceExec::from_data_source`. The memtable scan in the same file (`MemorySourceConfig`, near line 359) is not wrapped: it decodes nothing. |
| `query/traces.rs`, `file_plan` | same. Its `mem_plan` (near line 703, `MemorySourceConfig`) is intentionally not wrapped. |
| `compaction/merge.rs`, `file_scan` (near line 219) | same. The deletion rewriter (`compaction/rewrite.rs`) builds its scan through this `file_scan`, so this one change covers both. |
| `verify.rs`, `verify_file` | `guard(ParquetRecordBatchStreamBuilder::new_with_options(reader, options))`, `guard_sync(|| builder.build())`, and the stream in `GuardedStream::new(stream, || Err(decoder_panicked()))`. A panic gives `Verdict::Corrupt { reason: "decode failed" }` through the existing `failed`. |
| `anchor.rs`, `Anchors::rebuild_file` | same three wrappings; errors map through `AnchorError::parquet`. `rebuild` already skips and suspects a file on a non-store error. |
| `compaction/slice.rs`, `time_values` | same three wrappings; errors map through `parquet_error`, a `CompactionError::Read` that `may_be_corrupt` accepts, so the job suspects its inputs. |
| `wal/record.rs`, `read_batch` | `catch_unwind(AssertUnwindSafe(...))` around the `StreamReader::try_new` and `next()` calls; a panic is `WalError::InvalidPayload { reason: "ipc decoder panicked" }`. Replay then treats it as a corrupt record, and `skip_corrupt` works. |

Afterwards, `grep -rn -F -e "DataSourceExec::from_data_source" -e "ParquetRecordBatchStreamBuilder::new" -e "ParquetObjectReader::new" crates/storage/src`: every non-test hit outside `unwind.rs` is in the table above, or the memtable scan. Report any other hit; don't leave it unwrapped.

### Tests (`unwind.rs`)

All of these are plain `#[test]` functions that call `runtime().block_on(...)` (the fuzz entry's runtime), not `#[tokio::test]`: the helpers they reuse already block on that runtime.

A test-only `PanicStore` wraps `InMemory`: `head` and `put` delegate; `get_opts` with a range and `get_ranges` panic with `"test store panics on read"`. Implement `ObjectStore` the way `test_support::FaultStore` does (same method list). A panicking store is not a decoder panic, but it panics at the same place (inside the scan's poll), and it needs no upstream bug to reproduce.

1. `guard_sync_turns_a_panic_into_an_error`.
2. `a_panicking_scan_fails_the_query_and_suspects_its_file`: `parquet_bytes(FileTable::Spans)` (R3.2's builder) in a `PanicStore`, `parquet_query`, `span_list` → `Err`; `verify.take(10) == vec![FileId(Uuid::from_u128(1))]`.
3. `the_runtime_is_usable_after_a_caught_panic`: after test 2's failed query, a `parquet_query` over a plain `InMemory` with the same file, on the same runtime, returns every row.
4. `verify_reports_a_panicking_read_as_corrupt`: `verify_file` over the `PanicStore` → `Verdict::Corrupt { reason: "decode failed" }`.
5. `anchor_rebuild_skips_a_panicking_file`: `rebuild_file` over the `PanicStore` with `parquet_bytes(FileTable::Traces)` → `Err(AnchorError::Parquet(e))` with `!is_store_error(&e)`.
6. `time_values_reports_a_panic_as_a_corrupt_read`: `Err(CompactionError::Read { source })` with `may_be_corrupt(&source)`.
7. `a_panicking_ipc_payload_is_an_invalid_record`: only if an input that panics Arrow's IPC reader is known by then. Otherwise leave it out and say so in the report; the `wal_record` target looks for one, and R3.8 adds it as a regression.

8. `guard_changes_no_other_plan_node`: the optimized physical plan of a fixed spans read is unchanged except for the `GuardExec` line.
   - **Before writing any R3.3 code**, on R3.2's commit, build the plan with `parquet_query(InMemory with parquet_bytes(FileTable::Spans))`, then `ctx.session().sql("SELECT span_id FROM spans WHERE status = 'error' ORDER BY start_time DESC LIMIT 10")`, `.create_physical_plan()`, and render it with `displayable(plan.as_ref()).indent(false).to_string()`.
   - Strip each line's leading whitespace and paste the lines into the test as `const EXPECTED: &str`.
   - The test builds the same plan after R3.3, drops the lines that start with `GuardExec`, strips leading whitespace, and compares with `EXPECTED`. Any other difference (an added `RepartitionExec`, a lost limit or predicate, a second `CooperativeExec`) means the wrapper is not transparent: fix `GuardExec`, don't edit `EXPECTED`.

The default panic hook prints each caught test panic to stderr; the tests still pass.

**Existing tests that inspect plans.** These walk physical plans and downcast to `DataSourceExec`; run them first, they are the regression net for the wrapper:
- `compaction/merge.rs`, the test helper near lines 880–905;
- `deletion_tests.rs`, near line 99;
- `query/spans.rs`, near lines 658–665.

If one fails only because it now meets a `GuardExec` where it expected the `DataSourceExec`, make its walk descend into `children()` (or unwrap a `GuardExec` first) and keep every assertion. If one fails for any other reason, the wrapper changed the plan: fix `GuardExec`.

### Spec and changelog

- **`docs/spec/03-storage.md` § 10**, row "Corrupted Parquet file": after its first sentence add "A panic in the Parquet or Arrow decoder during any of these reads is caught at our scan (`src/unwind.rs`) and handled as a decode error." After the table, add: "The WAL reader treats a panic of Arrow's IPC decoder as a corrupt record (§1). The decoders are fuzzed (track R M3, `fuzz/`)."
- **`CHANGELOG.md`** `[Unreleased]` → `Fixed`: "A Parquet decoder panic on a corrupt data file no longer drops the request, stops compaction or prevents startup: the read fails and the file is verified and quarantined."

### Commands

```bash
cargo test -p cetra-storage --all-features unwind
just check
```

---

## R3.4 — `read_cache_block` and `catalog_rows`

Commit: `storage: add the read cache and catalog fuzz entry points (R3.4)`

### Dependencies

- Root `[workspace.dependencies]`: `arbitrary = { version = "1.5.0", features = ["derive"] }`, commented `# Structured fuzz inputs (track R M3). Storage uses it under cfg(fuzzing) and in tests.`
- `crates/storage/Cargo.toml`:

  ```toml
  # Fuzz entry points (track R M3): cargo-fuzz sets `--cfg fuzzing`.
  [target.'cfg(fuzzing)'.dependencies]
  arbitrary.workspace = true
  ```

  and `arbitrary.workspace = true` under `[dev-dependencies]`. Cargo evaluates `cfg(fuzzing)` against `RUSTFLAGS`, so normal builds don't compile it. `Cargo.lock` gains `arbitrary` and `derive_arbitrary`; `just deny` must still pass.

### Visibility

- `catalog/mod.rs` declares `mod db;` (private, line 9), so nothing in `db` is reachable from the crate root even when `pub(crate)`. Add to `catalog/mod.rs`:

  ```rust
  #[cfg(any(test, fuzzing))]
  pub(crate) use db::{check_file_sources, load, load_project_states, load_tombstones};
  ```

  and make `load_project_states` `pub(crate)` in `catalog/db.rs` (it is `pub(super)`). `load`, `load_tombstones` and `check_file_sources` are already `pub(crate)`. The entry calls them as `crate::catalog::load` and so on.
- `read_cache.rs`: `block_count` (near line 697) is private. Don't change it: the entry computes `u64::from(len).div_ceil(BLOCK)`; `BLOCK` is already `pub(crate)`. `ReadCache`, `CacheStore` and its fields are `pub(crate)`.

### `pub fn read_cache_block(data: &[u8])`

```rust
#[derive(Debug, arbitrary::Arbitrary)]
struct CacheInput {
    /// Object length: `1 + len % (BLOCK + BLOCK / 2)`, so at most two blocks.
    len: u32,
    size_file: SizeFile,
    blocks: [BlockFile; 2],
    /// At most 4 used. Start: `start % (len + 16)`; end: `start + 1 + width % (BLOCK + 8)`.
    reads: Vec<(u32, u32)>,
}

#[derive(Debug, arbitrary::Arbitrary)]
enum SizeFile { Missing, True, Text(Vec<u8>) }

#[derive(Debug, arbitrary::Arbitrary)]
enum BlockFile { Missing, Good, BadCrc, Truncated(u16), Extended(Vec<u8>), Raw(Vec<u8>) }
```

`CacheInput::arbitrary_take_rest(Unstructured::new(data))`; an `Err` returns.

1. The object's bytes are `content(len)`: byte `i` is `i % 251`, as `read_cache.rs::tests::content`. Put it into an `InMemory` at `tables/spans/project=p/day=2026-11-11/0-0192e5a0-7c1a-7000-8000-000000000001.parquet`; the cache name is the last segment.
2. In a scratch dir, write `cache/<name>/size` (`True` → `len` as decimal text; `Text(bytes)` as is; `Missing` → no file) and the block files `cache/<name>/<index>` of the blocks that exist (`index < u64::from(len).div_ceil(BLOCK)`):
   - `Good`: the block's bytes then their CRC-32C, little-endian (the layout `read_block` checks);
   - `BadCrc`: `Good` with the CRC's first byte flipped;
   - `Truncated(n)`: `Good` cut to `n % good.len()` bytes;
   - `Extended(extra)`: `Good` then `extra`;
   - `Raw(bytes)`: `bytes`.
3. `ReadCache::open(cache_dir, 64 MiB, Arc::new(Disk::open(vec![scratch], 0)?), 0)` and `CacheStore { inner: store, cache }`.
4. For each read: `get_opts` with `GetRange::Bounded(start..end)`, then `.bytes()`. **Property:** when `cache.size(name)` is `None` or `Some(len)` before the read and `start < len`, the result is `Ok` and equals `content[start..min(end, len)]`. When it is `Some(other)`, the result is not checked: a `size` file that parses to a wrong number is audit finding 4 (`00-design.md` § Audit so far), which M2 owns. **If M2 made the `size` file checked** (read `ReadCache::open` and `docs/research/storage/hardening.md` § M2), then `Some(other)` is itself a failure: also assert that `cache.size(name)` is `None` or `Some(len)` right after `open`.
5. Drop the store, `ReadCache::open` the same directory again, and repeat the reads under the same property.

### `pub fn catalog_rows(data: &[u8])`

```rust
#[derive(Debug, arbitrary::Arbitrary)]
enum Text { Id(u8), Project(u8), Day(u16), Table(u8), State(u8), Kind(u8), Status(u8), Raw(String) }

#[derive(Debug, arbitrary::Arbitrary)]
struct FileRow {
    id: Text, table: Text, project: Text, day: Text, level: i64, path: Text,
    size: i64, footer: i64, rows: i64, min_time: i64, max_time: i64,
    min_ingest: i64, max_ingest: i64, fully_merged: i64, state: Text, applied_seq: i64,
}

#[derive(Debug, arbitrary::Arbitrary)]
struct DeletionRow {
    id: Text, seq: i64, project: Text, kind: Text, cutoff: i64, status: Text, registered_at: i64,
    traces: Vec<Vec<u8>>, users: Vec<String>, sessions: Vec<String>,
}

#[derive(Debug, arbitrary::Arbitrary)]
struct CatalogInput {
    files: Vec<FileRow>,
    sequences: Vec<(Text, i64)>,
    /// Indexes into `files`: (traces file, spans file).
    sources: Vec<(u8, u8)>,
    deletions: Vec<DeletionRow>,
    watermarks: Vec<(Text, Text)>,
    dropped: Vec<Text>,
    now: i64,
}
```

`Text` renders as:
- `Id(n)`: `Uuid::from_u128(0x0192e5a0_7c1a_7000_8000_000000000000 | u128::from(n))`, hyphenated;
- `Project(n)`: `format!("018f6b4e-7c1a-7c1a-8c1a-0000000000{n:02x}")`;
- `Day(n)`: 2026-01-01 plus `n % 400` days, `%Y-%m-%d`;
- `Table(n)`: `["spans", "traces", "scores", "logs"][n % 4]`;
- `State(n)`: `["active", "obsolete", "quarantined"][n % 3]`;
- `Kind(n)`: `["traces", "user", "other"][n % 3]`;
- `Status(n)`: `["pending", "applied"][n % 2]`;
- `Raw(s)`: `s`.

Every `Vec` is cut to 8 entries, inner ones too.

Steps, inside `runtime().block_on`:
1. `SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true).connect()`, then `sqlx::migrate!("./migrations/catalog").run(&mut connection)`, the same macro call as `open_catalog_db`. Harness setup: `expect`.
2. Insert the rows with plain `sqlx::query(...).bind(...)`: `data_files` (`created_at` 0), `table_state`, `file_sources` (ids of `files[i % len]`), `deletions` and their `deletion_traces`, `deletion_users`, `deletion_sessions`, `retention_watermarks`, `projects` (`state` `"dropped"`, `dropped_at` 0). A failed insert (constraint, type) is ignored.
3. The open sequence, stopping at the first `Err`: `catalog::load`, `catalog::check_file_sources`, `catalog::load_tombstones`, `catalog::load_project_states`.
4. On success: `compaction::plan::plan(&catalog, &HashSet::new(), &Policy::from(&CompactionConfig::default()), input.now)`; drop the jobs.

### Tests

- `read_cache_block_corpus_replays`, `catalog_rows_corpus_replays`.
- `read_cache_serves_true_bytes_over_bad_blocks`: a hand-built `CacheInput` (`len` for 1.5 blocks, `True` size, blocks `[BadCrc, Truncated(10)]`, reads across the block boundary) through the entry's steps: the reads are `Ok` and correct.
- `catalog_rows_loads_a_consistent_catalog`: a hand-built input with one spans file, its traces file and their `file_sources` row, all from the valid `Text` variants: `load` returns 2 files and `check_file_sources` is `Ok`. Proves the generator can reach the success path.
- `write_seed_corpus` gains four seeds per target: 64 bytes of `0x00`, 64 of `0xff`, `0..=255`, and 1,024 bytes of `(i * 31) % 256`. `arbitrary` decodes any bytes; the fuzzer grows the rest. The two hand-built tests above build their inputs as structs, not from these files.

---

## R3.5 — OTLP targets

Commit: `otel: add the OTLP fuzz entry points (R3.5)`

`crates/otel/src/lib.rs` gets the `fuzz_entry` declaration of § Conventions. `crates/otel/src/fuzz_entry.rs`:

```rust
fn context() -> IngestContext {
    IngestContext {
        project_id: PROJECT,           // "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse()
        ingest_time: 1_767_225_600_000_000_000, // 2026-01-01, the fixtures' usual ingest time
        retention_days: None,
        limits: IngestLimits::default(),
    }
}

/// Built once: compiling the built-in price patterns per input would dominate.
fn prices() -> &'static PriceTable; // OnceLock; PriceTable::new(builtin_price_rows()), expect (harness setup)

pub fn otlp_http_protobuf(data: &[u8]);   // decode_protobuf, then normalize_and_convert
pub fn otlp_http_json(data: &[u8]);       // decode_json, then normalize_and_convert
pub fn otlp_grpc(data: &[u8]);            // decode_protobuf; on Ok, request.encoded_len()

fn normalize_and_convert(request: &ExportTraceServiceRequest);
```

`normalize_and_convert`: `let normalized = normalize(request, &context(), prices());`, then **property:** `cetra_core::batch::spans_to_record_batch(&normalized.spans)` is `Ok`. The server converts accepted spans the same way before the append (`IngestError::Batch` answers 500), so a span that `normalize` accepts but core cannot convert is a finding. If it fails in R3.8 and the fix belongs in `cetra-core`, stop and report (core is a shared contract).

Tests (`fuzz_entry::tests`; unit tests, because an integration test cannot see `cfg(test)` items):
- `otlp_http_protobuf_corpus_replays`, `otlp_http_json_corpus_replays`, `otlp_grpc_corpus_replays`, with the same `replay` helper as storage (copy it; the crates share no test code).
- `a_fixture_normalizes_to_spans`: one fixture's `input.json` through `decode_json` and `normalize` gives at least one span (the seeds reach real normalization).
- `write_seed_corpus` (`#[ignore]`): for every `fixtures/otlp/**/input.json`, sorted by path (writes all of them; R3.5 commits only some, below):
  - `otlp_http_json/seed-<group>-<case>`: the file's bytes;
  - `otlp_http_protobuf/seed-<group>-<case>` and `otlp_grpc/seed-<group>-<case>`: `decode_json(bytes)?.encode_to_vec()` (`prost::Message`; `prost` is already a dependency). Skip a fixture that does not decode.

`fixtures/otlp/` is 2.5 MiB, over the per-target budget. **R3.5 commits only a handful of seeds per target**: the seeds of the first fixture of each group directory (`edge`, `genai`, `google_adk`, `laminar`, `langfuse`, `cetra`, `openinference`, `openllmetry`, `vercel`), nine per target. Delete the other generated seed files before committing. R3.6 runs `just fuzz-seed` again before `just fuzz-all`, so every fixture seeds the first fuzz run, and `fuzz-cmin` keeps only what adds coverage; the minimized corpus is what R3.6 commits.

### Commands

```bash
just fuzz-seed
# keep the nine group seeds per otel target, delete the rest
cargo test -p cetra-otel --all-features fuzz_entry
```

Commit the kept seeds with this sub-phase.

---

## R3.6 — The `fuzz/` crate, recipes and supply chain

Commit: `fuzz: add the cargo-fuzz crate, recipes and its first corpus (R3.6)`

### Layout

```
fuzz/
  Cargo.toml
  Cargo.lock              generated by `just fuzz-lock`, not committed (below)
  .gitignore              target/  artifacts/  coverage/  Cargo.lock
  fuzz_targets/<target>.rs   one per target, nine
  corpus/<target>/        committed, minimized; what libFuzzer runs from
  regressions/<target>/   committed; created with a target's first crash (R3.8); replayed only by the stable tests
```

**Corpus and regressions are kept apart.** libFuzzer's panic hook aborts before unwinding, so an input that panics upstream crashes every libFuzzer run that loads it, guarded or not. The recipes therefore never pass `fuzz/regressions/` to libFuzzer; only the stable replay tests read it. An input that still panics upstream can't reach `corpus/` either: libFuzzer writes a crashing input to `fuzz/artifacts/`, never to a corpus directory, and `just fuzz-cmin` would itself crash on one, which is the check before any corpus is committed. A crash artifact goes to `regressions/` (R3.8), never to `corpus/`.

`fuzz/Cargo.toml`:

```toml
[package]
name = "cetra-fuzz"
version = "0.0.0"
edition = "2024"
publish = false
license = "MIT"

[package.metadata]
cargo-fuzz = true

# Its own workspace: outside the main one, nightly only (D-R-7).
[workspace]

[dependencies]
# Exact pin: `just fuzz-lock` resolves it on top of the root lock, and Dependabot bumps it here.
libfuzzer-sys = "=0.4.13"
cetra-otel = { path = "../crates/otel" }
cetra-storage = { path = "../crates/storage" }

[[bin]]
name = "wal_record"
path = "fuzz_targets/wal_record.rs"
test = false
doc = false
bench = false
# … one [[bin]] per target, same shape
```

The root `[workspace] members = ["crates/*", …]` does not match `fuzz/`, and `fuzz/`'s own `[workspace]` table stops Cargo from looking further up. The path dependencies resolve their `workspace = true` keys against the main workspace, which still owns them. `arbitrary` is not listed: the entries take bytes, storage pulls `arbitrary` itself under `cfg(fuzzing)`, and libfuzzer-sys depends on the same 1.x. The only fuzz-only pin is therefore `libfuzzer-sys`.

Each target is three lines:

```rust
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| cetra_storage::fuzz_entry::wal_record(data));
```

### Toolchain

The root `rust-toolchain.toml` pins stable 1.99.1. Every fuzz recipe calls `cargo +{{fuzz_toolchain}} fuzz …`; a `+toolchain` argument overrides the toolchain file. cargo-fuzz itself is built with stable. cargo-fuzz's defaults stay: release build with debug assertions, AddressSanitizer (the dependencies hold `unsafe` decoders), `--cfg fuzzing`.

### `fuzz/Cargo.lock`

**Not committed: generated from the root lock on every use.** It is a pure function of the root `Cargo.lock` and `fuzz/Cargo.toml`:

```just
# Write fuzz/Cargo.lock: the root lock's versions, plus the exact libfuzzer-sys pin
fuzz-lock:
    cp Cargo.lock fuzz/Cargo.lock
    cargo metadata --manifest-path fuzz/Cargo.toml --format-version 1 > /dev/null
```

`cargo metadata` (stable) keeps every locked version the fuzz graph uses, drops the rest, and adds `libfuzzer-sys` at its exact pin. Its other dependencies are already in the root lock: `arbitrary` (after R3.4) and the build-time `cc` 1.11.1, `jobserver`, `shlex`, `find-msvc-tools`. So the lock holds exactly the shipped versions plus one crate, and nothing is lost by regenerating it. `deny`, `deny-advisories`, `fuzz`, `fuzz-ci` and `fuzz-cmin` depend on `fuzz-lock` (`recipe: fuzz-lock`).

Why not commit it:
- A committed copy goes stale on every root dependency change and on every `just release-prep` (the path crates' version changes). Either `cargo deny --locked` fails, or the deny run rewrites the lock and `just assert-clean` fails, on every root Dependabot PR and every release, until someone runs `just fuzz-lock`.
- Generated each time, it can't drift. `cargo deny` checks exactly what would be fuzzed: the root's versions plus `libfuzzer-sys`. Dependabot keeps the exact pin in `fuzz/Cargo.toml` current, and the root entry keeps everything else current.
- Reproducing an old fuzz run needs only that commit's root lock.

`fuzz-lock` reads the crates.io index for `libfuzzer-sys` when the local cache lacks it, so it needs the network the first time, as any build does.

### `cargo deny`

- In the `justfile`, `deny` and `deny-advisories` each get a second line for the fuzz graph:

  ```just
  deny: fuzz-lock
      cargo deny check advisories bans licenses sources
      cargo deny --manifest-path fuzz/Cargo.toml check advisories bans licenses sources
  ```

  (`deny-advisories: fuzz-lock`: the same with `advisories` only.) In cargo-deny 0.20.2, `--manifest-path` and `--config` are top-level options, before `check`; `check` has no `--config`. Pass no `--config`: `just` runs from the repository root, where cargo-deny finds `deny.toml`. **Check this**: the fuzz run's output must show the licence exception below being used (or `licenses ok`), not a default config. If it doesn't pick up the root `deny.toml`, add the top-level `--config deny.toml` before `check` and report it. No `--locked`: the lock was just generated.
- `deny.toml`: libfuzzer-sys is `(MIT OR Apache-2.0) AND NCSA` (it bundles LLVM's libFuzzer), and NCSA is not in `allow`. Add a narrow exception rather than widening the list:

  ```toml
  # libFuzzer (LLVM, NCSA) inside the fuzz-only crate libfuzzer-sys; fuzz/ is never shipped (track R M3).
  [[licenses.exceptions]]
  crate = "libfuzzer-sys"
  allow = ["NCSA"]
  ```
  The root-graph run then warns that this exception matches no crate (`libfuzzer-sys` is only in the fuzz graph). That is a warning, not an error, and is expected. Don't silence it by moving the exception into a second config file.
- The family bans apply as they are: the fuzz graph resolves one version of each DataFusion-family crate, copied from the root lock. Run `just deny` and report its output if anything other than `ok` and the expected warning comes back.

### Formatting and lints

- `fmt`: add `cargo fmt --manifest-path fuzz/Cargo.toml`; `fmt-check`: `cargo fmt --manifest-path fuzz/Cargo.toml -- --check`. Both run on stable.
- **No clippy on `fuzz/`.** Linting it needs a second full build of DataFusion with another lock and target directory, for nine three-line files. The code that matters, the entries, is linted in their crates (`cfg(test)`). The nightly's build catches a target that stops compiling.

### `justfile` recipes

```just
fuzz_toolchain := "nightly-2026-11-03"
fuzz_targets := "wal_record wal_replay parquet_file read_cache_block catalog_rows filter_dsl otlp_http_protobuf otlp_http_json otlp_grpc"

# Install the fuzzing toolchain and cargo-fuzz (track R M3)
fuzz-tools:
    rustup toolchain install {{fuzz_toolchain}} --profile minimal
    cargo install cargo-fuzz --version 0.13.2 --locked

# Fuzz one target for `secs` seconds; new inputs go into its committed corpus
fuzz target secs="60": fuzz-lock
    cargo +{{fuzz_toolchain}} fuzz run {{target}} fuzz/corpus/{{target}} -- -max_total_time={{secs}} -rss_limit_mb=4096 -timeout=30 -max_len=$(just _fuzz-max-len {{target}})

# Fuzz every target in turn; fails if any target crashed
fuzz-all secs="60":
    #!/usr/bin/env bash
    set -uo pipefail
    rc=0
    for target in {{fuzz_targets}}; do just fuzz "$target" {{secs}} || rc=1; done
    exit "$rc"

# Minimize every corpus, then check the 2 MiB budget
fuzz-cmin:
    …
```

- `_fuzz-max-len target` (marked `[private]`): prints `1048576` for `parquet_file`, `262144` for `wal_replay`, `otlp_http_protobuf`, `otlp_http_json` and `otlp_grpc`, `65536` for the others. libFuzzer's default is the largest seed or 4 KiB, too small for Parquet.
- `fuzz-cmin` (bash, `fuzz-cmin: fuzz-lock`): for each target, `cargo +{{fuzz_toolchain}} fuzz cmin <target> fuzz/corpus/<target> -- -max_len=<its max len>`. It renames the files to content hashes (the reason for § Conventions' seed builders). Then two budget checks, each failing with a message naming the target:
  - size: `du -sb fuzz/corpus/<target>` at most 2,097,152 bytes (`"<target> corpus is over 2 MiB"`);
  - count: at most `_fuzz-max-files <target>` files (`"<target> corpus has over N files"`), so the stable replay stays under 10 s in debug. Start with 300 for `parquet_file`, `filter_dsl` and `catalog_rows` (each input runs DataFusion or SQLite) and 2,000 for the others. Expect the measured caps to end well below that for the DataFusion targets: at several to tens of milliseconds per input in debug, 10 s allows on the order of 100. R3.6 and R3.8 set the numbers from measured replay times and report them.

  A corpus still over a budget after `cmin` loses its largest files (`ls -S`) until it fits, and the report names how many.

### `.github/dependabot.yml`

Add after the root cargo entry, with the same schedule, cooldown, limit, labels and commit prefix:

```yaml
  - package-ecosystem: cargo
    # The fuzz crate (track R M3). Its shared crates come from the root lock
    # through `just fuzz-lock`; Dependabot updates only the exact libfuzzer-sys pin.
    directory: /fuzz
    allow:
      - dependency-name: "libfuzzer-sys"
    schedule: …          # the root cargo entry's, copied
    cooldown: …
    open-pull-requests-limit: 5
    labels:
      - dependencies
    commit-message:
      prefix: deps
```

The DataFusion-family ignores are not needed there: `allow` already excludes everything else. With no lockfile in `/fuzz`, Dependabot updates the manifest requirement itself. Dependabot reads path dependencies outside `/fuzz` from the repository; whether it resolves their `workspace = true` keys is unverified. Check the first scheduled run on GitHub's Dependabot page; if it fails to parse, remove the entry and report it.

### `AGENTS.md`

- **§ Rules → Dependencies**, new bullet: "`fuzz/` is a cargo-fuzz crate outside the workspace (D-R-7). It pins its one fuzz-only crate, `libfuzzer-sys`, exactly in `fuzz/Cargo.toml`, and gets everything else through path dependencies on the workspace crates. Storage takes `arbitrary` from the root `[workspace.dependencies]` as usual. `fuzz/Cargo.lock` is not committed: `just fuzz-lock` generates it from the root lock (the fuzz recipes and `just deny` run it first), so the fuzzed versions are the shipped ones. `just deny` checks both graphs."
- **§ Rules → Tests**, new bullet: "A fuzz crash is fixed with its input in `fuzz/regressions/<target>/` and a named unit test. `just test` replays every corpus and regression on stable. Regressions are never passed to libFuzzer, and a crashing input never goes into `fuzz/corpus/`."
- **§ Layout**: `fuzz/  cargo-fuzz targets (nightly only), committed corpus and regressions (track R M3)` after `tools/e2e/`.
- **§ Commands**: rows for `just fuzz <target> [secs]`, `just fuzz-all [secs]`, `just fuzz-cmin`, `just fuzz-seed`, `just fuzz-lock`, `just fuzz-tools`; under "Required tools", "for fuzzing: the nightly toolchain and cargo-fuzz 0.13.2 (`just fuzz-tools`)".

### First corpus

```bash
just fuzz-tools
just fuzz-seed
just fuzz-all 60
just fuzz-cmin
just check
```

`fuzz-all 60` grows each corpus from the seeds. A crash here is a finding for R3.8: copy the artifact (`fuzz/artifacts/<target>/crash-*`) to the scratchpad, name it in the report, don't commit it, and go on. Commit the minimized corpora (not `fuzz/Cargo.lock`, which is ignored). Report each target's corpus size and file count, and the replay test times.

---

## R3.7 — The nightly workflow

Commit: `ci: add the nightly fuzz, model, stress and LazyFS workflow (R3.7)`

What runs nightly (user decision): the nine fuzz targets, `just model 5000` (M1, D-R-14), M6's stress test (`just stress 1800`, and `CETRA_STRESS_REMOTE=1 just stress 600` on the in-memory store with the read cache), and M2's `just lazyfs` if M2's probe showed `/dev/fuse` works on runners. M5's disk-full tests do **not** run nightly.

**Who adds the stress jobs.** Whichever of M3's R3.7 and M6's R6.7 lands second adds them, and only if they are absent. If R3.7 lands first and `just stress` doesn't exist yet, leave the two jobs out and say so in the report; R6.7 adds them (06-m6-concurrency.md § Nightly). If `just stress` exists, add them here, and R6.7 then finds them present.

### `.github/workflows/nightly.yml`

Outside `ci-ok`, like `packaging.yml`. Follow AGENTS.md § CI: actions pinned by commit SHA with the tag in a comment (reuse the SHAs `ci.yml` and `release.yml` pin: `actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1`, `actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9 # v7.0.2`, `taiki-e/install-action@b80e2bb2635f28eb98b9fce1e2f0887e1535c6b2 # v2.90.0`); every `run:` step calls a `just` recipe; `persist-credentials: false`; `permissions: contents: read` at the top, widened only in the job that needs it.

```yaml
name: Nightly

on:
  schedule:
    - cron: "23 2 * * *" # 02:23 UTC daily; off the hour to avoid the scheduler rush
  workflow_dispatch:

permissions:
  contents: read

concurrency:
  group: nightly
  cancel-in-progress: false

env:
  CARGO_TERM_COLOR: always
  CARGO_INCREMENTAL: "0"
  RUST_BACKTRACE: short
  TMPDIR: /dev/shm

jobs:
  fuzz:
    name: fuzz (${{ matrix.target }})
    runs-on: ubuntu-24.04
    timeout-minutes: 45
    strategy:
      fail-fast: false
      matrix:
        target: [wal_record, wal_replay, parquet_file, read_cache_block, catalog_rows,
                 filter_dsl, otlp_http_protobuf, otlp_http_json, otlp_grpc]
    steps:
      - checkout (persist-credentials: false)
      - uses: ./.github/actions/rust   # with cache: "false"; same zizmor comment as ci.yml
      - run: just fuzz-tools
      - run: just fuzz-ci ${{ matrix.target }} 600
      - upload-artifact, if: always():
          name: fuzz-${{ matrix.target }}
          path: |
            target/fuzz-new/${{ matrix.target }}
            fuzz/artifacts/${{ matrix.target }}
          if-no-files-found: ignore
          retention-days: 14

  model:
    runs-on: ubuntu-24.04
    timeout-minutes: 300
    steps:
      - checkout; ./.github/actions/rust (cache: "false")
      - run: just model 5000
      - upload-artifact, if: failure(): crates/storage/proptest-regressions/ (name: model-regressions)

  stress:
    runs-on: ubuntu-24.04
    timeout-minutes: 60
    steps:
      - checkout; ./.github/actions/rust (cache: "false")
      - run: just stress 1800

  stress-remote:
    # M6's remote mode: the in-memory object store behind the 64 KiB read cache.
    runs-on: ubuntu-24.04
    timeout-minutes: 60
    env:
      CETRA_STRESS_REMOTE: "1"
    steps:
      - checkout; ./.github/actions/rust (cache: "false")
      - run: just stress 600

  # lazyfs: see below

  report:
    needs: [fuzz, model, stress, stress-remote]      # plus lazyfs when it exists
    if: failure()
    runs-on: ubuntu-24.04
    timeout-minutes: 5
    permissions:
      contents: read      # the checkout; a job-level block replaces the top-level one
      issues: write
    steps:
      - checkout (persist-credentials: false)
      - taiki-e/install-action with tool: just@1.59.0
      - run: just nightly-issue
        env:
          GH_TOKEN: ${{ github.token }}
          RUN_URL: ${{ github.server_url }}/${{ github.repository }}/actions/runs/${{ github.run_id }}
```

The YAML abbreviates repeated steps; write them in full, as `ci.yml` does. `${{ matrix.target }}` in a `run:` line is a fixed list of our own names, not user input.

- **`fuzz-ci target secs`** (recipe, `fuzz-ci target secs: fuzz-lock`): like `fuzz`, but it first runs `mkdir -p target/fuzz-new/<target>` (libFuzzer needs every corpus directory to exist), and that empty directory is the first corpus directory, followed by `fuzz/corpus/<target>`. Never `fuzz/regressions/` (R3.6 § Layout). libFuzzer writes new inputs into the first directory only, so the artifact holds exactly the new corpus entries and the checkout stays clean. A crash exits non-zero and leaves `fuzz/artifacts/<target>/crash-*`.
- **Stress jobs.** Exactly M6's commands: `just stress 1800`, and `just stress 600` with `CETRA_STRESS_REMOTE=1` (set through `env:`). The remote mode uses an in-memory store, so no secrets are needed. The seed is printed in the log (`CETRA_STRESS_SEED`). A job timeout of 60 minutes each, as M6 sets. When the jobs are absent (see above), drop them from `report.needs` too.
- **`nightly-issue`** (recipe, bash): reads `RUN_URL`; finds an open issue labelled `nightly` (`gh issue list --label nightly --state open --json number --jq '.[0].number // empty'`); comments `"Nightly failed again: $RUN_URL"` on it, or creates one titled `Nightly run failed` with that body and `--label nightly` (create the label first with `gh label create nightly --force`). One open issue, updated each failing night. The run URL comes in through `env`, never through `${{ }}` inside the script (zizmor's template-injection rule).
- **`model`** runs D-R-14's nightly 5,000 cases. If M1 measured `just model 5000` at over 4 hours, split it into a matrix of shards, each `just model <5000 / shards>` (each run draws its own random seed), and report the split.
- **`lazyfs`.** M2 probes `/dev/fuse` on a throwaway branch and records the result. Read `docs/research/storage/hardening.md` § M2 (LazyFS) and M2's recipes:
  - **If M2's probe showed that `/dev/fuse` works on GitHub's `ubuntu-24.04` runners:** add a `lazyfs` job running the recipe(s) M2 documents (its install recipe, then `just lazyfs`), with `timeout-minutes` at M2's measured duration plus half, and add it to `report.needs`.
  - **Otherwise:** no job. Add to `hardening.md` § M3: "LazyFS does not run nightly: `/dev/fuse` is unavailable on GitHub runners (M2); it runs on the soak machine (D-R-5)."

**Budget.** The repository is public, so standard GitHub-hosted runners cost no minutes.
- **Fuzz jobs.** Each of the nine builds DataFusion from scratch with sanitizer coverage and AddressSanitizer, uncached (no kache: another toolchain and other flags). The build time is unknown until measured; ASan release builds of this graph may well take 20–30 minutes on a 4-core runner. **The first `workflow_dispatch` run decides each job's `timeout-minutes`**: set it to the measured build plus 10 minutes of fuzzing plus 50 %. Until then, use 60.
- **The other jobs:** the model job takes its measured length; the stress jobs take 30 and 10 minutes plus their release builds.
- **Artifacts** are a few MiB, kept 14 days.
- Record the measured durations in `hardening.md` § M3 *Runs*. If the fuzz builds dominate, a later change can build once and share the binaries; that is not part of M3.

### `AGENTS.md` § CI

Add: "`.github/workflows/nightly.yml` runs nightly and on demand, outside `ci-ok`: each fuzz target for 10 minutes, the 5,000-case model test, the stress test (30 minutes locally, 10 minutes remote), and the LazyFS check when runners allow it (track R). A failure opens or updates one issue labelled `nightly`."

### Commands

```bash
just check-workflows
just check
```

After merge, start the workflow once by hand (`workflow_dispatch`) and record its result and durations in `hardening.md` § M3 *Runs* (R3.8).

---

## R3.8 — Long runs and fixes

Commit per bug: `<crate>: <fix in imperative> (R3.8)`. A final docs commit: `docs: record the M3 fuzz runs (R3.8)`.

1. Fuzz every target for at least one hour locally, in the background with output in a log file (`just fuzz <target> 3600 > <scratchpad>/fuzz-<target>.log 2>&1`); a foreground command times out after 10 minutes. Run at most half as many targets at once as the machine has cores (each process also runs DataFusion threads).
2. **For each crash** (`fuzz/artifacts/<target>/crash-*`, `oom-*`, `timeout-*`, or a stack overflow):
   1. Minimize it: `cargo +<fuzz_toolchain> fuzz tmin <target> <artifact>`.
   2. Copy the minimized input to `fuzz/regressions/<target>/<short-name>`, never to `fuzz/corpus/` (R3.6 § Layout).
   3. Add a named unit test in the owning `fuzz_entry::tests` (`regression_<target>_<what>`), calling the entry on that file. On stable it fails before the fix, unless step 4's second case applies.
   4. Decide whose bug it is:
      - **Our code panics or breaks a property:** find the cause. If the fix is local (one module, no format, public API or spec change), fix it in the same commit as the regression. Otherwise stop and report the input, the cause and the change it needs.
      - **An upstream panic caught by R3.3's guard** (the stable regression test passes with no fix): not ours to fix. Keep the regression and its test (they check the guard). Report the panic upstream (arrow-rs, prost, serde_json, sqlx …) with the minimized input; fuzz inputs never carry user data. Record the issue link. It stays a crash under libFuzzer (user decision), so the target keeps finding it until upstream releases a fix, and the nightly's one `nightly` issue stays open with a link to the upstream report. When the fixed release lands in the root lock, the regression stays as a test.
      - **An upstream panic outside the guard:** move or add the guard (R3.3's table) so it covers that call, in the same commit as the regression. A call that cannot be guarded locally is a stop-and-report.
      - **An OOM or a timeout:** decide whether production can reach it. A request-sized OTLP input that needs over 30 s or 4 GiB is ours to bound. A Parquet file is written by us: an OOM on a corrupt one is reported like an upstream panic.
   5. Add a row to `hardening.md` § M3 *Findings*: id `M3-<n>`, target, regression file, cause, classification (ours, upstream guarded, upstream unguarded, resource), fix commit or upstream link.
3. `just fuzz-cmin`, commit the corpora (within both budgets), and check that each replay test still runs in under 10 s in debug. If one does not, lower that target's `_fuzz-max-files`, rerun `fuzz-cmin`, and report the new number.
4. List the deferred targets in `hardening.md` § M3 (user decision: later): the SQL console sandbox and the server's body decompression. Also list the residual of R3.3: the default panic hook prints upstream panic messages.
5. **`docs/research/storage/hardening.md`**, new section `## M3 — Fuzzing`:
   - this plan's targets table (target, what it reaches, properties checked);
   - the panic boundary (R3.3) in two sentences;
   - `### Runs`: date, commit, target, duration, executions, corpus size, result. The R3.6 runs, then the R3.8 hour-long runs, then the first nightly;
   - `### Findings`: the rows of step 2, or "None yet."
6. Update track R's row in `docs/roadmap.md`: "M3 done".

If a crash's cause is not found within the code its stack trace touches, stop and report. Don't guess.

---

## What not to build in M3

- **More targets:** the SQL console's sandbox (`sql/validate.rs`), the server's body decompression (`crates/server/src/ingest/body.rs`, outside R's scope), media blobs, the committer's writes. Candidates for later, listed in `hardening.md`.
- **Structure-aware Parquet generation** (valid files built from `arbitrary` batches with the Arrow writer). The seeds already start from valid files, and generated files test the writer more than the reader.
- **A guard around every DataFusion operator.** Only decoders of stored bytes get the boundary; a panic in DataFusion's own operators on valid data is a DataFusion bug like any other.
- **A server panic hook**, or `panic = "abort"`.
- **Coverage reports, OSS-Fuzz, other sanitizers, LibAFL.** cargo-fuzz's defaults are enough to start.
- **Clippy for `fuzz/`** (R3.6).
- **A committed `fuzz/Cargo.lock`** (R3.6: it would go stale on every root dependency change and every release).
- **M5's disk-full tests in the nightly** (user decision).
