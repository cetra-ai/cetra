# cetra workload · M1: harness and the likely failures

Implementation plan for milestone M1 of [08-cetra-workload-design.md](08-cetra-workload-design.md). Revision 3, after two cold reviews.

M1 builds a new benchmark mode, `fmtbench cw`, that compares Strata with cetra's Parquet files without DataFusion. It then runs it on the cases most likely to rule Strata out: map-key explosion, payloads with no repetition, extreme span shapes, and search needles (short, common, rare, absent, non-ASCII). M1.1 to M1.3 run in parallel in one worktree (§ Phase graph), each done by one agent in one go; M1.4 joins them. Each ends with the checks green and one commit. M1.5 is the runs and the report, done by the orchestrating session.

**Prerequisites:** the orchestrator commits this plan, the design doc and `strata-readiness.md` before cutting the phase branches, so every worktree has them. The bench crate builds (see § Build).

**Read first:**
- [08-cetra-workload-design.md](08-cetra-workload-design.md): § Architecture, § Decisions (CW-1, CW-3, CW-7, CW-12, CW-16 to CW-19), § M1.
- `bench/src/main.rs`:
  - `write_parquet` (the `pq_cetra` branch) and `write_strata`: the writers M1 uses. `cw` is a child module of the crate root, so it calls these private functions as `crate::write_parquet` and `crate::write_strata`.
  - `Location`, `location`, `open_strata`, `open_pq_fast`, `read_text`, `hex`, `parse_id`: reused as they are.
  - `generate`: how spans become sorted Arrow slices. M1.1 does the same in its own function.
- `bench/src/counting.rs`: `Counting::reset` and `Counting::stats` give GETs and bytes.
- `bench/src/pqfast.rs`: `open` and `lookup`, the Parquet point lookup M1.3 reuses; `PqFile`'s `store`, `path` and `meta` are public, its file size is not.
- `testdata/src/datagen.rs`: `World::new`, `World::trace`, `Rng` (`new`, `next`, `below`, `range`, `unit`) and `preview`. `Vocab::new` and `World`'s vocabulary are private: M1.1 has its own word generator.
- `crates/core/src/span.rs` (field names) and `crates/core/src/batch.rs` (`spans_to_record_batch`).
- The `strata` API: `StrataFile::scan_batches(&ScanRequest, threads)` returns `ScanBatches`, an iterator of `Result<RecordBatch>`; `StrataFile::lookup_scan(key, projection, views)`; `ScanRequest` (all fields public; set `limit: None`, `top: None`, `kind: ScanKind::Scan`, `row_ranges: None`); `RowFilter { columns, mask }`; `Predicate::{Or, Contains, MapKeyEq}`; `Lit::Utf8`; `StrataFile::io_stats().waves`.

**Conventions.**
- `fmtbench` is a research tool: `unwrap` and `expect` are allowed, `unsafe` is not.
- Every new item has a doc comment.
- Errors use the crate's `R<T>` (`Box<dyn Error + Send + Sync>`).
- No new dependency (§ Versions). Tests make their directories under `std::env::temp_dir()` with a unique name (`cw-test-<test>-<process id>`), removed first if present; there is no `tempfile` crate.
- The crate is on edition 2024, where `gen` is a reserved word: the module of cases is `cases`.
- `cw` items that name crate-private types (`Location`, `PqFile`) are `pub(crate)`; the others may be `pub` or `pub(crate)`.

## What it is, and what it is not

**When M1 is done:**
- `fmtbench cw gen <case> <dir>` writes one of nine generated cases, and `fmtbench cw import <src> <dir>` turns an existing dataset's `pq_cetra` files into a case.
- `fmtbench cw run <dir|s3://…>` writes both formats (local only), runs every query of the case in fresh processes with the formats alternating, checks that both formats return the same answer, and prints median, p95 and p99 latency, CPU, GETs, bytes, waves and peak RSS, with a verdict per query.
- The runs of M1.5 give M1's verdicts, written up in `strata-readiness.md`.

**Not in M1:**
- the `traces` and `scores` tables and day partitions (M3, CW-19);
- compaction and write breakdowns (M2);
- any change to Strata. Failures found here get a fix plan of their own (CW-5), as earlier milestones did (`01f-m1f-fixes.md`).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| CW-1 | The baseline is `pq_cetra`, read without DataFusion. |
| CW-3 | Each query is written once per format, with the same algorithm. |
| CW-7 | A query wins only when Strata is faster at the median and the p95. |
| CW-12 | Strata must also have smaller files and a lower peak RSS. |
| CW-16 | A write wins only when it beats Parquet on wall time and on CPU time. |
| CW-17 | Search is `ILIKE '%q%'` over `input`, `output`, `name`, `system_instructions`, with arrow's `ilike` kernel. Strata's index is case-sensitive, so it is used only for needles without cased characters. |
| CW-18 | Each measured run is a fresh process per query, format and round; the formats alternate. |
| CW-19 | M1 writes `spans` files only, four per case. |

User answers recorded in the design doc: strict wins on speed, size and memory (CW-7, CW-12); GDPR deletion out of scope (CW-6).

## Versions

No new dependency. M1 uses what `bench/Cargo.toml` already has, at the versions of `bench/Cargo.lock`: `arrow` and `parquet` 59.3.2 (`ipc` for the input files, `arrow::compute::kernels::comparison::ilike` for search), `serde_json` 1, `xxhash-rust` 0.8 (`xxh3_64`), `cpu-time` 1.0.0, `tokio`, `futures`. The implementer reports the versions in `bench/Cargo.lock` that the build used.

## Build

`bench`, `testdata` and `strata` point `cetra-core` at `/home/sebastien/travaux/llm-observer/crates/core`, whose `Span` has fields this worktree's `testdata` doesn't set. The orchestrator points them at this worktree's `crates/core` before the phases start, and puts the paths and the lock files back after M1.4. **Phases never edit or commit `Cargo.toml` or `Cargo.lock` files** (M1.4 edits only `bench/Cargo.toml`'s `[features]`); if a build needs a change there, stop and report it.

Commands run in `docs/research/file-format/bench/`, with `LIBCLANG_PATH=/usr/lib/llvm-18/lib` set: Vortex's `custom-labels` dependency runs bindgen, which doesn't find libclang on this machine otherwise.

## Phase graph

| Phase | Edit | Depends on |
|---|---|---|
| M1.1 cases | `bench/src/main.rs`, `bench/src/cw/mod.rs`, `bench/src/cw/cases.rs` | — |
| M1.2 harness and writes | `bench/src/cw/harness.rs` | — |
| M1.3 readers and queries | `bench/src/cw/read.rs` | — |
| M1.4 wiring | `bench/Cargo.toml` (`[features]` only), `bench/src/main.rs`, `bench/src/cw/mod.rs`, `bench/src/cw/harness.rs` | M1.1, M1.2, M1.3 |
| M1.5 runs and report | `strata-readiness.md`, `plans/08-cetra-workload-design.md` (orchestrator) | M1.4 |

**M1.1, M1.2 and M1.3 run in parallel**, in the session's one worktree, on the branch `claude/file-format-research`, so that they share one `target/` and the dependencies (DataFusion, Vortex, arrow) build once instead of three times:
- The orchestrator's base commit adds `mod cw;` to `main.rs`, a `cw/mod.rs` that declares each module behind a temporary feature of the `bench` crate (`#[cfg(feature = "cw_cases")] pub mod cases;`, `cw_harness`, `cw_read`), and the three features in `bench/Cargo.toml`. Each phase builds and tests with its own feature only (`--features cw_cases` and so on), so another phase's unfinished file is never compiled. A feature of the root crate doesn't change its dependencies, so they build once.
- Each phase owns one new file, and its tests build what they need without the other two (M1.2 and M1.3 write their test files with `spans_to_record_batch`, `crate::write_parquet` and `crate::write_strata`, not with M1.1's cases).
- Only M1.1 edits `main.rs` (the `Some("cw")` arm and `USAGE`) and `cw/mod.rs` (`cmd`, which it puts under `#[cfg(feature = "cw_cases")]` too).
- Each phase commits only its own files: `git add <its new files>`, then `git commit -m "<subject>" -- <its files>`. Nothing else in the working tree is theirs to stage (it holds other uncommitted work).
- M1.4 runs after the three: it removes the features (plain `pub mod` lines, nothing left in `Cargo.toml`), wires `cw run`, `cw one` and the readers together, and runs the end-to-end checks.

While several agents run, each builds and tests only `bench` and only its own tests (`cargo test --release --features cw_<module> cw::<module>::`). Cargo's lock on the shared `target/` makes the builds take turns.

---

## M1.1: cases

**Goal:** generate the nine M1 cases, and import real datasets, as sorted Arrow input files plus the text files the queries need.

**Edit:** `bench/src/main.rs` (only the `Some("cw")` arm, under `#[cfg(feature = "cw_cases")]`, and the `USAGE` text), `bench/src/cw/mod.rs` (add `cmd`), `bench/src/cw/cases.rs` (new).

### Contracts

**Command line** (`cw/mod.rs`, dispatched from `run()` in `main.rs` as `Some("cw") => cw::cmd(&args[2..]).await`):

```text
fmtbench cw gen <case> <dir> [--traces N]
fmtbench cw import <src-dir> <dir> [name=needle ...]
```

`USAGE` gains: ` | cw gen <case> <dir> [--traces N] | cw import <src-dir> <dir> [name=needle...] | cw run <dir|s3://bucket/key> [--rounds R] [--runs K] [--queries a,b]` (M1.4 wires `cw run`; the usage text goes in now).

```rust
/// `fmtbench cw …`: the cetra workload benchmark (plans/08-m1-likely-failures.md).
pub async fn cmd(args: &[String]) -> crate::R<()>;
```

**The case directory** (`<dir>`), written by `gen` and `import`:

| Path | Content |
|---|---|
| `input/000.arrow` … `input/003.arrow` | Arrow IPC files (`arrow::ipc::writer::FileWriter`), each holding one or more `RecordBatch`es whose rows, in order, are sorted by `(trace_id, start_time)`: the rows both formats write, one output file per input file. `gen` writes one batch per file; `import` writes one input file per source file, with the source's batches as they are (a whole `wildchat` file concatenated could pass the 2 GiB `Utf8` offset limit). |
| `cw_queries.txt` | The case's queries, one per line, in run order (table below). |
| `cw_traces.txt` | 20 trace ids, lowercase hex, one per line. |
| `cw_spans.txt` | 20 lines `<trace hex> <span hex>`: spans whose `input` is not null. |
| `cw_needles.txt` | Lines `<name>\t<needle as lowercase hex of its UTF-8 bytes>`. Absent when the case has no search. |
| `cw_attr.txt` | One line `<key>\t<value>` for `attr_filter`. Only for the `keys*` cases. |

Query names: `write`, `scan_payload`, `trace_light`, `span_heavy`, `attr_filter`, `attr_keys`, and `search:<name>` for each line of `cw_needles.txt`.

**Cases** (`cw gen`):

| Case | Traces (default) | Start | Change | Queries, in order |
|---|---|---|---|---|
| `keys100` | 100,000 | `World` traces, payloads stripped (`crate::strip_payloads`) | `attributes` of every span: 20 distinct keys from a pool of 100 | `write`, `attr_filter`, `attr_keys`, `trace_light` |
| `keys1k` | 100,000 | same | pool of 1,000 | same |
| `keys5k` | 100,000 | same | pool of 5,000 | same |
| `text_unique` | 10,000 | `World` traces | every non-null `input` and `output` replaced by unique text of the same byte length; needles planted | `write`, `scan_payload`, every `search:*`, `span_heavy` |
| `b64` | 10,000 | same | replaced by base64 of random bytes; needles planted | same |
| `json_random` | 10,000 | same | replaced by random JSON; needles planted | same |
| `nonllm95` | 10,000 | `World` traces | 19 telemetry spans added under each LLM span; needles planted | `write`, `trace_light`, `scan_payload`, every `search:*` |
| `giant` | 2,000 | `World` traces | every 40th LLM span gets a 7,500,000-byte `input`; needles planted | `write`, `span_heavy`, `scan_payload`, `search:rare`, `search:absent` |
| `intl` | 10,000 | `World` traces | every non-null `input` and `output` replaced by multilingual text; needles planted | `write`, every `search:*` |

`--traces N` overrides the trace count (tests use small counts). Generation always uses `World::new(7)` and `Rng::new(42)`, so a case is the same on every machine.

**Constants** (`cw/cases.rs`):

```rust
/// Input files per generated case (CW-19).
pub const FILES: usize = 4;
/// Keys per span in the `keys*` cases.
pub const KEYS_PER_SPAN: usize = 20;
/// Distinct values per key in the `keys*` cases: `"v0"` … `"v49"`, JSON-quoted.
pub const KEY_VALUES: usize = 50;
/// Telemetry spans added under each LLM span in `nonllm95`.
pub const TELEMETRY_PER_LLM: usize = 19;
/// Every this-many-th LLM span of `giant` gets a giant input.
pub const GIANT_EVERY: usize = 40;
/// Bytes of a giant input: under the 8 MiB attribute cap of D40.
pub const GIANT_BYTES: usize = 7_500_000;
/// The absent needle: planted nowhere.
pub const ABSENT: &str = "qzxjabsentneedle";
/// Planted needles: (query name, variants used in turn, plant in every n-th input, offset).
pub const PLANTS: [(&str, &[&str], usize, usize); 5] = [
    ("rare", &["zqxjrareneedle"], 10_000, 3),
    ("common", &["commonneedle"], 10, 1),
    ("bug", &["bug", "Bug", "BUG"], 100, 7),
    ("404", &["404"], 100, 57),
    ("sql", &["sql", "Sql", "SQL"], 100, 31),
];
/// What each planted query searches for (`cw_needles.txt`), with `absent` first.
pub const NEEDLES: [(&str, &str); 6] = [
    ("absent", ABSENT),
    ("rare", "zqxjrareneedle"),
    ("common", "commonneedle"),
    ("bug", "bug"),
    ("404", "404"),
    ("sql", "SQL"),
];
/// Phrases of the `intl` text: accents, CJK, emoji, Cyrillic, and case pairs.
pub const INTL_PHRASES: [&str; 10] = [
    "Le café est très chaud", "CAFÉ CRÈME à emporter", "東京の天気は晴れです", "数据库连接错误",
    "🚀 déploiement terminé ✅", "Die Straße ist gesperrt", "STRASSE GESPERRT",
    "Ошибка подключения к базе данных", "naïve résumé façade", "😀😃 emoji 🎉",
];
/// Extra needles of `intl`, appended to `NEEDLES`.
pub const INTL_NEEDLES: [(&str, &str); 5] = [
    ("cafe", "café"), ("tokyo", "東京"), ("rocket", "🚀"), ("strasse", "straße"), ("oshibka", "ошибка"),
];
```

**Base64** (full body; standard alphabet, `=` padding, no line breaks):

```rust
/// Standard base64 of `bytes`, with `=` padding.
fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { A[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { A[n as usize & 63] as char } else { '=' });
    }
    out
}
```

### Steps

1. **`cmd`** in `cw/mod.rs` (the base commit already declares the module), which matches `args[0]`: `"gen"` → `cases::gen_cmd`, `"import"` → `cases::import_cmd`, anything else → `Err(crate::USAGE.into())`.
2. **Text generators** (`cw/cases.rs`), each returning a `String` of exactly `len` bytes (cut at the last char boundary at or below `len`; for `intl` and `json_random`, at most `len` bytes):
   - `unique_text(rng, len)`: words of 3 to 10 letters (`rng.range(3, 10)`; `Rng::range` includes both ends), each letter `b'a' + rng.below(26)`, separated by single spaces, the first letter of every 12th word uppercased.
   - `b64_text(rng, len)`: `base64` of `len * 3 / 4 + 3` bytes, each `rng.next() as u8`, cut to `len`.
   - `json_text(rng, len)`: `{"id":"<32 hex>","items":[` then `{"k":"<16 hex>","v":<rng.next() % 1_000_000>},` repeated while the string is shorter than `len`, then the last comma replaced by `]}` (or `]}` appended when there is none), then cut to at most `len` bytes (the text is ASCII, so any byte is a boundary). Below about 60 bytes the result is a cut prefix, not valid JSON; that is fine. Hex digits come from `rng.next()`.
   - `intl_text(rng, len)`: `INTL_PHRASES[rng.below(10)]` joined by `" "` while shorter than `len`, then cut at a char boundary to at most `len`.
3. **Mutations** of a span list:
   - `replace_payloads(spans, f)`: for every span, each of `input` and `output` that is `Some(s)` becomes `f(rng, s.len())`; then `input_format`/`output_format` = `Some(PayloadFormat::Text)` where the value is `Some`, and `input_preview`/`output_preview` = `datagen::preview(&value)`.
   - `keys(spans, pool)`: the pool is `pool` keys; key `i` is `format!("tool.arg.{i}")` for even `i` and 32 hex digits from `rng` for odd `i`. Every span's `attributes` is replaced by `KEYS_PER_SPAN` distinct keys drawn with `rng.below(pool)` (draw again on a repeat), each with value `format!("\"v{}\"", rng.below(KEY_VALUES))`. `cw_attr.txt` gets `tool.arg.2\t"v3"`.
   - `telemetry(spans)`: after each span of kind `SpanKind::Llm`, append `TELEMETRY_PER_LLM` clones with: `span_kind = SpanKind::Span`; name `"db.query"` for even clone index, `"http.get"` for odd; a fresh `span_id` (`SpanId::from_bytes(rng.next().to_be_bytes())`, drawing again while it fails); `parent_span_id = Some(llm.span_id)`; `start_time = llm.start_time + 1_000 * (j + 1)`, `end_time = start_time + 500`; `attributes` = `{"db.system": "\"postgresql\""}` or `{"http.method": "\"GET\""}` (same parity); `status = SpanStatus::Unset`, `status_message = None`, `events = vec![]`; `None` for every field from `provider` to `tool_definitions` in `Span`'s declaration order that is an `Option` (tokens, costs, models, payloads, formats, previews, `ttft_ns`, `request_params`, `system_instructions`, `tool_definitions`). The share of LLM spans is then about 5%.
   - `giant(spans)`: count LLM spans in order from 0; the ones with `count % GIANT_EVERY == 0` and a `Some` input get `input = giant_text(rng, &old_input, count / GIANT_EVERY)`, where an even index repeats `old_input` (joined by `"\n"`) and cuts the result at the last char boundary at or below `GIANT_BYTES` (so it is 0 to 3 bytes shorter), and an odd index, or an empty `old_input`, is `b64_text(rng, GIANT_BYTES)`. Set `input_format` and `input_preview` as in `replace_payloads`. The giant spans' `(trace, span)` pairs are kept for `cw_spans.txt`.
   - `plant(spans)`: walk the spans whose `input` is `Some`, counting them from 0 as `k`. For each entry of `PLANTS`, when `k % every == offset`, prepend `"<variant> "` to the input, the variant being `variants[(k / every) % variants.len()]`. Several plants may land on one input.
4. **Signatures** used by the commands and the tests:

   ```rust
   /// The text files of a case directory, in memory.
   pub struct CaseFiles {
       pub queries: Vec<String>,
       pub traces: Vec<[u8; 16]>,
       pub spans: Vec<([u8; 16], [u8; 8])>,
       /// (name, needle) as plain text; the file holds the needle in hex.
       pub needles: Vec<(String, String)>,
       pub attr: Option<(String, String)>,
   }
   /// The spans and text files of a generated case (`--traces` = `traces` when given).
   pub fn gen_spans(case: &str, traces: Option<usize>) -> crate::R<(Vec<Span>, CaseFiles)>;
   /// Writes the input files (step 6) and the text files of a case into `dir`.
   pub fn write_case(dir: &Path, spans: Vec<Span>, files: &CaseFiles) -> crate::R<()>;
   ```

   `gen_cmd` is `gen_spans` then `write_case`; tests may change the spans or the files in between.

   **`gen_cmd(args)`**: parse `<case> <dir> [--traces N]`. Generate traces with `world.trace(&mut rng, &mut spans)`; for the `keys*` cases call `crate::strip_payloads` on each trace's spans as `generate` does. Apply the case's mutation, then `plant` for every case but the `keys*` ones. Pick ids with `rng`:
   - `cw_traces.txt`: 20 distinct trace ids of random spans;
   - `cw_spans.txt`: for `giant`, up to 10 giant spans then random spans with an input up to 20; otherwise 20 random spans with an input.
   
   Then write the input files (step 6) and the text files. `cw_needles.txt` holds `NEEDLES`, plus `INTL_NEEDLES` for `intl`, for every case but the `keys*` ones; `giant`'s `cw_queries.txt` lists only `search:rare` and `search:absent` of them. Print the span count, LLM share, and each input file's rows. In `giant` (about 10,000 inputs at 2,000 traces) the rare needle lands in about 2 inputs.
5. **`import_cmd(args)`**: `<src-dir> <dir> [name=needle ...]`. Read every `<src-dir>/pq_cetra/*.parquet` in path order with `parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder`, and write each file's batches, as they come, to one input file, in order (the files are already sorted). Ids: `cw_traces.txt` = the first 20 lines of `<src-dir>/trace_ids.txt`; `cw_spans.txt` = rows `i * rows / 20` of the first input file, moving forward to the next row with a non-null `input` (stop at 20 or at the file's end). Needles: the arguments when given; otherwise `absent=ABSENT`, `404=404`, `bug=bug`, `sql=SQL`, `the=" the "`, and `rare=<first line of <src-dir>/needles.txt, which is already hex>` when that file exists. Queries: `write`, `scan_payload`, `trace_light`, `span_heavy`, then every `search:*`.
6. **Input files** (`write_inputs(dir, spans)`): sort the spans by `start_time`, cut them into `FILES` slices of `len.div_ceil(FILES)`, and for each slice build batches of 8,192 spans with `spans_to_record_batch`, concatenate, sort by `(trace_id, start_time)` with `lexsort_to_indices` and `take_record_batch` (as `generate` does), and write `input/NNN.arrow` with that one batch.

### Tests (`cw/cases.rs`, `#[cfg(test)]`)

- `base64_matches_known_vectors`: `""`, `"f"`, `"fo"`, `"foo"`, `"foob"`, `"fooba"`, `"foobar"` → `""`, `"Zg=="`, `"Zm8="`, `"Zm9v"`, `"Zm9vYg=="`, `"Zm9vYmE="`, `"Zm9vYmFy"`.
- `text_generators_hit_their_length`: each of the four generators for lengths 0, 1, 7, 1,000 and 100,000: at most `len` bytes, valid UTF-8, and exactly `len` for `unique_text` and `b64_text`.
- `unique_text_repeats_no_window`: 50 texts of 4,096 bytes from one `rng`: no 32-byte window occurs in two of them.
- `planting_hits_its_rates`: 20,000 spans with an input: `zqxjrareneedle` in 2, `commonneedle` in 2,000, and each of `bug`/`Bug`/`BUG` in about a third of 200.
- `telemetry_makes_95_percent_non_llm`: `gen nonllm95 --traces 200`: the LLM share is under 6% and every telemetry span has a parent and no payload.
- `keys_cases_have_twenty_distinct_keys`: `keys1k --traces 50`: every span has 20 distinct keys; the spans use at most 1,000 distinct keys in all; every even-numbered key seen is `tool.arg.<even number below 1,000>`.
- `giant_case_has_giant_inputs`: `giant --traces 200`: at least one input of at least `GIANT_BYTES - 3` bytes (a planted needle may lengthen it), and `cw_spans.txt` starts with giant spans.
- `gen_writes_a_case_directory`: `text_unique --traces 30` into a temp dir: four input files whose rows are sorted by `(trace_id, start_time)` and add up to the span count; the four text files (`cw_queries`, `cw_traces`, `cw_spans`, `cw_needles`) parse, and there is no `cw_attr.txt`.
- `keys_case_writes_its_attr_file`: `keys100 --traces 20`: `cw_attr.txt` is `tool.arg.2\t"v3"`.

### Done when

- `cargo build --release --features cw_cases` and `cargo test --release --features cw_cases cw::cases::` pass.
- `cargo run --release --features cw_cases -- cw gen keys100 /tmp/cw-k --traces 1000` and `… cw gen giant /tmp/cw-g --traces 100` print plausible counts.

**Commit:** `Add fmtbench cw cases: generated and imported spans`

---

## M1.2: harness and writes

**Goal:** run a case's queries in fresh processes, alternating formats, and report latency, CPU, I/O and peak RSS with a verdict; the `write` query is the first one measured.

**Edit:** `bench/src/cw/harness.rs` (new).

### Contracts

**Command line:**

```text
fmtbench cw run <dir|s3://bucket/key> [--rounds R] [--runs K] [--queries a,b] [--out FILE]
fmtbench cw one <dir|s3://bucket/key> <format> <query> <K>      (spawned by `run`; not for people)
```

Defaults: `R = 3`, `K = 3`, queries = `cw_queries.txt`, `--out` = `cw-<unix seconds>.jsonl` in `std::env::temp_dir()` (never in the repository); `run` prints the path. Formats are `pq_cetra` and `st`. `write` is skipped for an `s3://` location.

**One process's result:** `cw one` prints its result as one line of JSON built with `serde_json::json!`, the only stdout line that starts with `{`. Other lines (the size tables of `write`; what `location()` prints under `FMT_SIM_S3` or `FMT_HEDGE_MS`; the profile `main()` prints after `run()` under `FMT_PPROF`, which comes after the JSON) are not part of the result; `run` prints them through.

```json
{"query":"search:bug","format":"st","runs_ms":[12.1,11.8,12.0],"cpu_ms":[40.2,39.9,40.0],
 "gets":152,"bytes":1048576,"waves":3,"rows":417,"answer":"9f1c2a…","rss_start":24117248,
 "vmhwm":98123776,"bytes_written":null,"tail_bytes":null}
```

- `runs_ms`, `cpu_ms`: one entry per measured run, wall and process CPU time in ms.
- `gets`, `bytes`: `Counting::stats` read right after the first measured run (the counters were reset after the warm-up). Not comparable between formats, so not judged: Parquet's reader hands `get_ranges` many page ranges, which `Counting` counts one by one before the store merges them, while Strata merges its ranges first.
- `waves`: the sum of `io_stats().waves` over the Strata files, read at the same moment; `null` for Parquet. After the warm-up, Strata's block tables and dictionaries are cached and Parquet's footers were read at open, so neither figure includes a cold query's metadata reads; M3 measures cold queries.
- `rows`, `answer`: from the query (M1.3's `Answer`); `answer` is 16 lowercase hex digits.
- `rss_start`: `VmRSS` in bytes when the process starts, kept in the `.jsonl` for reference; `vmhwm`: `VmHWM` in bytes when it ends, which the verdict uses. Both are read from `/proc/self/status` (the `kB` value × 1024). For `write`, `vmhwm` includes the input batches loaded before the runs, the same for both formats.
- `bytes_written`: total bytes of the written files for `write`, else `null`.
- `tail_bytes`: for `write`, the metadata each format reads at open, summed over the files: Strata's `FileSummary::tail_len`; for Parquet, the footer (the 4-byte little-endian length before the closing `PAR1`, plus 8) plus the column and offset indexes that `pqfast::open` loads (`column_index_length()` and `offset_index_length()` of every column chunk in the file's metadata, read back with `ParquetMetaDataReader`). Else `null`.

**Summary** (`run` prints one line per query and format, then the verdict):

```text
search:bug    pq_cetra  median 15.20  p95 17.80  p99 18.10  cpu 60.0  gets 180  KiB 2048.0  waves -  rss_max 120.3 MiB  rows 417
search:bug    st        median 12.00  p95 13.10  p99 13.50  cpu 40.0  gets 152  KiB 1024.0  waves 3  rss_max  93.6 MiB  rows 417
search:bug    verdict   WIN (median, p95, rss)
```

`write` lines add `size MiB` (from `bytes_written`) and its verdict judges median, p95, CPU median, peak RSS and size (CW-12, CW-16).

```rust
/// The p-th percentile of `v` (0 < p <= 1) by nearest rank: the value at sorted
/// position `ceil(p * n) - 1`. `v` must not be empty.
fn percentile(v: &[f64], p: f64) -> f64;

/// Whether Strata wins: every compared figure strictly lower than Parquet's.
/// Figures: median and p95 latency and max VmHWM; for `write`, also the median CPU time
/// and the bytes written. Returns the names of the figures Strata loses on (empty = win).
fn losses(query: &str, st: &Summary, pq: &Summary) -> Vec<&'static str>;
```

`losses` names the figures `"median"`, `"p95"`, `"rss"`, `"cpu"` and `"size"`, in that order.

```rust
/// One `cw one` process's result: the JSON line above.
pub struct ProcResult {
    pub query: String,
    pub format: String,
    pub runs_ms: Vec<f64>,
    pub cpu_ms: Vec<f64>,
    pub gets: u64,
    pub bytes: u64,
    pub waves: Option<u64>,
    pub rows: u64,
    pub answer: String,
    pub rss_start: u64,
    pub vmhwm: u64,
    pub bytes_written: Option<u64>,
    pub tail_bytes: Option<u64>,
}
impl ProcResult {
    /// The JSON line (`serde_json::json!(…).to_string()`).
    pub fn to_line(&self) -> String;
    /// Parses a line written by `to_line`.
    pub fn from_line(line: &str) -> crate::R<Self>;
}

/// A format's results for one query, merged over all rounds.
pub struct Summary {
    /// Every measured run of every round.
    pub runs_ms: Vec<f64>,
    pub cpu_ms: Vec<f64>,
    /// From the first round.
    pub gets: u64,
    pub bytes: u64,
    pub waves: Option<u64>,
    pub rows: u64,
    pub answer: String,
    pub bytes_written: Option<u64>,
    pub tail_bytes: Option<u64>,
    /// The largest `vmhwm` of all rounds.
    pub vmhwm_max: u64,
}
impl Summary {
    /// Merges the results of one format, in round order. `results` is not empty.
    pub fn from_results(results: &[ProcResult]) -> Summary;
}

/// The two formats in the order round `r` runs them.
pub fn formats_for_round(r: usize) -> [&'static str; 2];

/// `cw run`'s arguments (after `run`): spawns the processes, checks answers, prints the
/// summaries and verdicts. Returns an error after all queries if any answer mismatched.
pub async fn run_cmd(args: &[String]) -> crate::R<()>;

/// `cw one`'s arguments (after `one`): runs `one` and prints its line.
pub async fn one_cmd(args: &[String]) -> crate::R<()>;

/// Runs `query` K times in this process, as `cw one` does, and returns its result.
pub async fn one(dir: &str, format: &str, query: &str, runs: usize) -> crate::R<ProcResult>;

/// `one` for `write`: writes the input files of the local case `dir` as `format`, `runs` times.
pub fn write_format(dir: &Path, format: &str, runs: usize) -> crate::R<ProcResult>;
```

### Steps

1. **`run`**: parse the arguments. For each query, for each round `r` in `0..R`, run the formats in the order `formats_for_round(r)`: `[pq_cetra, st]` for even `r`, `[st, pq_cetra]` for odd `r`. Each run spawns `std::env::current_exe()` with `["cw", "one", dir, format, query, K]`, inherits the environment, waits, and fails the whole command if the exit status is not success. Print its other stdout lines, and its stderr, through. Parse its line that starts with `{` with `ProcResult::from_line` (exactly one, else the command fails), and append it, with `"round": r` added, to `--out`.
2. **Answers.** After all rounds of a query, every result of both formats must have the same `answer` and `rows`. Otherwise print `<query> ANSWER MISMATCH` with both formats' values, finish the remaining queries, and exit with code 1 at the end. `write` compares nothing.
3. **Summaries.** Print the lines above, then the verdict: `WIN (…)` listing the figures compared, or `LOSS (…)` listing `losses`.
4. **`write_format`** (local only): read `rss_start`; read every `input/NNN.arrow` into its batches first; then run K times: delete and recreate `<dir>/<format>/`, and for each input file `input/NNN.arrow` write its batches, in order, into one output file `<format>/NNN.parquet` or `<format>/NNN.strata` (same `NNN`; `open_pq_fast` and `open_strata` select files by extension). For Parquet, concatenate the file's batches when they all fit (as `gen` writes them, one batch) and call `crate::write_parquet("pq_cetra", &batch, path)`; for Strata, call `crate::write_strata` the same way. Both writers take one batch; when an input file holds several batches whose concatenation fails on the 2 GiB offset limit, the command fails with `input file NNN is too large for one batch: write the dataset with more, smaller files (fmtbench gen … <files>) and import it again`. Time each run with `Instant` and `ProcessTime`, and sum the file sizes. `rows` = input rows; `answer` = the hex of `xxh3_64(b"")` (writes compare equal); `gets`, `bytes` = 0. `write` is the one query without a warm-up run (CW-18's warm-up is for reads). `write_strata` builds the whole file in memory (`StrataWriter::finish` returns it as a `Vec<u8>`) while Parquet's writer streams to disk; that is how the two writers work today, and the RSS figure shows it. After the last run, compute `tail_bytes`, and print the size tables to stdout: for Strata, `crate::payload_table(dir, &summaries)` and `crate::narrow_table(&summaries)` from the last run's summaries (they show each payload column's frames, index and chunk store against Parquet's column sizes, which `payload_table` reads from `<dir>/pq_cetra`). The `.jsonl` keeps the figures; the tables are for M1.5's report.
5. **`one`** calls `write_format` for `write`. Any other query is an error `unknown query <name>` until M1.4 wires the readers in.
6. **Strata settings on S3.** Nothing in `cw`: the orchestrator sets `FMT_POINT_GAP=262144 FMT_POINT_MAX_RANGE=1048576` for S3 runs, which `crate::open_strata` already reads.

### Tests (`cw/harness.rs`)

- `percentile_is_nearest_rank`: `[1..=20]` → p50 = 10, p95 = 19, p99 = 20; one value → itself.
- `losses_lists_each_figure`: Strata lower on everything → empty; equal median → `["median"]` (a tie is a loss); higher p95 → `["p95"]`; higher RSS → `["rss"]`; `write` with more bytes → `["size"]`, with more CPU → `["cpu"]`; CPU is not compared for other queries.
- `rounds_alternate_formats`: the order function gives `pq_cetra, st, st, pq_cetra, pq_cetra, st` for three rounds.
- `result_line_round_trips`: `ProcResult::from_line(&r.to_line())` gives back every field, `null`s included.
- `write_writes_both_formats`: a test helper writes 20 `World` traces (`World::new(7)`, `Rng::new(42)`) as four input files, sorted as M1.1 § Steps 6 describes, into a temp dir; then `write_format(dir, "pq_cetra", 1)` and `write_format(dir, "st", 1)`: both format directories hold four files, `bytes_written` is their total, and `tail_bytes` is positive and smaller than `bytes_written`.

### Done when

- `cargo build --release --features cw_harness` and `cargo test --release --features cw_harness cw::harness::` pass. (The end-to-end run is M1.4's.)

**Commit:** `Add fmtbench cw run: fresh processes, alternating formats, verdicts`

---

## M1.3: readers and queries

**Goal:** every M1 query on both formats, with the same algorithm and the same parallelism, returning the same answer.

**Edit:** `bench/src/cw/read.rs` (new).

### Contracts

```rust
/// A query's result, compared across formats: a row count and a 64-bit hash of a canonical
/// encoding of the rows (below).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Answer { pub rows: u64, pub hash: u64 }

/// The files of one format, opened once per process.
pub enum Files {
    Parquet(Vec<crate::pqfast::PqFile>),
    Strata(Vec<Arc<strata::StrataFile>>),
}

/// The inputs of the queries, from the case's text files (M1.4 reads them).
pub struct Case {
    /// `cw_traces.txt`.
    pub traces: Vec<[u8; 16]>,
    /// `cw_spans.txt`.
    pub spans: Vec<([u8; 16], [u8; 8])>,
    /// `cw_needles.txt`: (name, needle).
    pub needles: Vec<(String, String)>,
    /// `cw_attr.txt`: (key, value).
    pub attr: Option<(String, String)>,
}

/// How a Strata search ran on one file: with the n-gram index on its payload columns, or as a
/// plain scan of every `SEARCH` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchPath { Index, Scan }

/// Runs `query` on `files`. `search:<name>` looks the needle up in `case.needles`.
/// A Strata search prints to stderr how many files took each `SearchPath`.
pub async fn run(query: &str, files: &Files, case: &Case) -> crate::R<Answer>;

/// The search of `needle`, with the path each Strata file took (empty for Parquet).
pub async fn search(files: &Files, needle: &str) -> crate::R<(Answer, Vec<SearchPath>)>;

/// The search pattern: `%`, the needle with `\` → `\\`, `%` → `\%`, `_` → `\_`, `%`.
pub fn pattern(needle: &str) -> String;

/// The rows of `batch` that match `pattern` on any `SEARCH` column it holds (found by name):
/// `ilike` per column, nulls made false (`arrow::compute::prep_null_mask_filter`), then
/// `arrow::compute::or`. Both formats' filters call it.
pub fn search_mask(batch: &RecordBatch, pattern: &str) -> Result<BooleanArray, ArrowError>;

/// Parquet's units for one file: `workers` contiguous row ranges of about equal size, each
/// as (row groups it overlaps, `RowSelection` over those row groups' rows).
fn parquet_units(row_group_rows: &[usize], workers: usize) -> Vec<(Vec<usize>, RowSelection)>;
```

**Canonical encodings** (hash = `xxh3_64` of the bytes):

| Query | Rows | Bytes hashed |
|---|---|---|
| `search:*`, `attr_filter` | matching spans | the matches sorted by `(trace_id, span_id)`, each as its 16 + 8 bytes |
| `attr_keys` | distinct keys | the keys sorted bytewise, each as its bytes, `0x00`, then its row count as `u64` little-endian |
| `scan_payload` | non-null `input` and `output` values | for every row, in file order and row order: `xxh3_64(input)` then `xxh3_64(output)`, little-endian, with `xxh3_64(b"\xff")` for a null (units are concatenated in row order, so both formats hash the same sequence) |
| `trace_light` | rows found | for each trace id in `case.traces` order: its rows' `span_id`s sorted, 8 bytes each |
| `span_heavy` | pairs found | for each pair in `case.spans` order that is found: `xxh3_64(input)` then `xxh3_64(output)`, little-endian, with `xxh3_64(b"\xff")` for a null |

**Columns.**
- `HEAVY = ["input", "output", "system_instructions", "tool_definitions", "attributes", "resource_attributes", "events", "input_preview", "output_preview"]`.
- `trace_light` projects every schema field not in `HEAVY`, by index, in schema order.
- `SEARCH = ["input", "output", "name", "system_instructions"]` (spec 05 § Full-text search).

**Search semantics** (CW-17): `search_mask` with `pattern(needle)`. `ilike` is `arrow::compute::kernels::comparison::ilike(&column, &Scalar::new(StringArray::from(vec![pattern])))`, and the column must be `Utf8`: Strata scans use `views: false`.

**`attr_filter` semantics** (as `Predicate::MapKeyEq`): a row matches when its `attributes` map holds the key exactly once, with a non-null value equal to the value.

**Parquet readers.** They use min/max statistics and the page index, as `pqfast` does; Parquet's bloom filters (on `trace_id`, `session_id`, `user_id`) are not read in M1, where every lookup is on the sorted `trace_id` of four files. They matter across many files (M3). `PqFile`'s file size is private: build each reader with `ParquetObjectReader::new(file.store.clone(), file.path.clone())` and `ParquetRecordBatchStreamBuilder::new_with_metadata(reader, file.meta.clone())`, which needs no HEAD. Batch size 8,192 on both formats. An `ArrowPredicateFn` closure is `FnMut(RecordBatch) -> Result<BooleanArray, ArrowError> + Send + 'static`: move an owned `pattern` (or key and value) into it.

### Steps

1. **Parallelism, the same thread budget on both formats.** With `files` files and `cores` from `std::thread::available_parallelism`, every file gets `workers = max(1, cores / files)`:
   - Strata: one task per file, `scan_batches(&req, workers)`;
   - Parquet: `workers` tasks per file, one per unit of `parquet_units(row group sizes, workers)`, each a stream builder with `.with_row_groups(unit.0)` and `.with_row_selection(unit.1)`.
   
   Tasks run with `tokio::spawn`, joined with `futures::future::try_join_all`. Lookups (`trace_light`, `span_heavy`) take one id after the other, each id on all files at once. The read verdict compares latency and RSS (CW-7, CW-12); CPU time is printed for every query but judged only for writes (CW-16).
2. **`search`.**
   - **Parquet:** output projection `trace_id`, `span_id` only; a `parquet::arrow::arrow_reader::RowFilter` with one `ArrowPredicateFn` whose `ProjectionMask` holds the `SEARCH` roots and which returns `search_mask(batch, &pattern)`. The text columns are decoded once, by the filter.
   - **Strata, scan path**, when the needle has a cased character (`needle.chars().any(|c| c.to_lowercase().ne(c.to_uppercase()))`), is longer than 64 bytes (`Predicate::Contains` takes at most 64), or the file has none of `input`, `output`, `system_instructions` as a payload column: `scan_batches` with projection `trace_id`, `span_id` and the `SEARCH` columns, no predicate, and `search_mask` applied by the caller to each batch. This is the work Parquet's filter does, with Strata's own parallel scan; Strata's `RowFilter` is not used, because it reads 65,536-row windows one after another, each in its own wave of requests. Path `Scan`.
   - **Strata, index path**, otherwise. A file's payload columns are those whose `DirectoryRecord` (from `file.directory()`, matched by `column_id` = schema index) has `format!("{:?}", record.layer) == "Payload"`: `Layer` is not exported but derives `Debug`. Then: (a) `predicate = Predicate::Or` of `Predicate::Contains(index, needle bytes)` for the `SEARCH` columns that are payload columns in this file, projection `trace_id`, `span_id`; (b) a scan of the other `SEARCH` columns (`name`, and any of the three that is not a payload column here) as in the scan path, with `search_mask` on them. The file's matches are the union of (a) and (b), deduplicated. Path `Index`. An error from (a) fails the query.
3. **`attr_filter`.** Parquet: output projection `trace_id`, `span_id`; a `RowFilter` over `attributes` that applies the semantics row by row (`MapArray::value(i)` gives the row's entries as a `StructArray` of keys and values). Strata: `predicate = Predicate::MapKeyEq(attributes index, key, Lit::Utf8(value))`, projection `trace_id`, `span_id`.
4. **`attr_keys`.** Both: project `attributes` alone and count, per key, the rows whose map holds it at least once; merge the units' counts.
5. **`scan_payload`.** Both: project `input`, `output`; hash every value as the encoding says, and count the non-null ones.
6. **`trace_light`.** Parquet: `pqfast::lookup(file, id, &light_roots)` on every file. Strata: `file.lookup_scan(id, &light_indexes, false)` on every file. Collect the `span_id`s.
7. **`span_heavy`.** For each pair, the trace's rows with projection `span_id`, `input`, `output` (Parquet: `pqfast::lookup`; Strata: `lookup_scan`), then the row whose `span_id` is the pair's. Both formats read the heavy columns of the whole trace, so a `giant` trace decodes all its giant inputs; M4 measures one span's columns alone.

### Tests (`cw/read.rs`)

A test helper `fixture(name, spans) -> PathBuf` writes `spans` into a temp dir as two files per format: the spans sorted by `start_time` and cut in half, each half built with `spans_to_record_batch`, sorted by `(trace_id, start_time)`, and written with `crate::write_parquet("pq_cetra", …)` to `pq_cetra/00N.parquet` and `crate::write_strata` to `st/00N.strata`. Tests open the files with `crate::location`, `crate::open_pq_fast(&loc, "pq_cetra")` and `crate::open_strata(&loc, "st")`. Spans come from `World::new(7)` and `Rng::new(42)`, changed by hand in each test. A **reference** is computed from the spans in memory, per row and per column, with `ilike` on a one-value `StringArray` (not with `search_mask`), and from the maps directly for the attribute queries.

- `search_agrees_with_reference`: 60 traces; `zqxjrareneedle` prepended to 2 inputs, `bug`, `Bug` and `BUG` to 3 outputs, `404` to one span's `name`, `SQL` into one `system_instructions`. Every non-null `input` and `output` is made at least 300 bytes (by repeating it) so that both are payload columns in Strata (`payload_min_avg` is 256); `system_instructions` already averages more. For needles `qzxjabsentneedle`, `zqxjrareneedle`, `bug`, `404`, `SQL`: both formats equal the reference; absent has 0 rows, rare 2, `bug` at least 3, `404` at least 1.
- `search_matches_despite_nulls`: spans whose `input` matches and whose `output` and `system_instructions` are null are matches on both formats.
- `search_agrees_on_non_ascii`: inputs set to `"CAFÉ CRÈME"`, `"Le café"`, `"東京の天気"`, `"🚀 done"`, `"Ошибка базы"`; needles `café`, `東京`, `🚀`, `ошибка`: both formats equal the reference. Print whether `café` matched `CAFÉ` (arrow's behavior, recorded in the report).
- `search_escapes_like_characters`: one input `"price 50%_off\\x now"`: `50%_off\x` gives 1 row on both formats; `50%off` gives 0 (unescaped, its `%` would match `50%_off`).
- `search_takes_the_index_only_without_cased_characters`: on `search_agrees_with_reference`'s files, `404` gives `[Index, Index]`, `bug` gives `[Scan, Scan]`, and a needle of 65 digits gives `[Scan, Scan]`.
- `attr_queries_agree`: 200 traces whose `attributes` are 20 keys from a pool of 100 (`tool.arg.<i>`), values `"v0"` … `"v49"`: `attr_filter(tool.arg.2, "v3")` and `attr_keys` equal across formats and to the reference; `attr_keys` has as many keys as the spans use.
- `attr_filter_ignores_other_keys`: a span holding the value under another key only is no match.
- `payload_and_lookups_agree`: 80 traces, 5 traces ids and 10 span pairs picked from them: `scan_payload`, `trace_light`, `span_heavy` equal across formats and to the reference; `span_heavy` finds 10 pairs.
- `parquet_units_cover_every_row`: row groups `[100_000, 100_000, 37]` and `[5]`, workers 1, 3 and 8: the units select every row exactly once, in order.

### Done when

- `cargo build --release --features cw_read` and `cargo test --release --features cw_read cw::read::` pass.

**Commit:** `Add fmtbench cw readers: search, attributes, payload scans and lookups on both formats`

---

## M1.4: wiring

**Goal:** connect the commands, the harness and the readers, and check the whole path end to end.

**Edit:** `bench/Cargo.toml` (remove the `[features]` section), `bench/src/main.rs` (remove the `cfg` on the `cw` arm), `bench/src/cw/mod.rs`, `bench/src/cw/harness.rs`.

### Steps

1. `cw/mod.rs` declares `pub mod cases; pub mod harness; pub mod read;` with no `cfg`, `cmd` loses its `cfg`, and `cmd` adds `"run"` → `harness::run_cmd(&args[1..])` and `"one"` → `harness::one_cmd(&args[1..])`.
2. In `harness.rs`, add

   ```rust
   /// The case's text files (M1.1 § Contracts), read through `loc`'s store with `crate::read_text`:
   /// hex ids parsed with `crate::parse_id` (spans: the 8-byte id from its 16 hex digits),
   /// needles decoded from hex to UTF-8 with `crate::profile::unhex`. A missing file gives an
   /// empty list or `None`.
   pub(crate) async fn read_case(loc: &crate::Location) -> crate::R<crate::cw::read::Case>;
   ```

   In `harness::one`, a query other than `write`: open the files once (`crate::open_pq_fast(&loc, "pq_cetra")` → `read::Files::Parquet`, or `crate::open_strata(&loc, "st")` → `read::Files::Strata`), read the case with `read_case`, run `read::run` once as a warm-up, then `loc.store.reset()` and each Strata file's `reset_io_stats()`, then K measured runs. Every run's answer must equal the warm-up's, else the process fails with `answer changed between runs`. `rows` and `answer` come from `read::Answer` (the hash as 16 lowercase hex digits).

### Tests

- `cw_case_round_trips` (`harness.rs`): `cases::write_case` of `cases::gen_spans("keys100", Some(20))` in a temp dir; `read_case` gives back its 20 traces, 20 spans and attribute pair. The same for `text_unique`, whose needles come back as plain text equal to `CaseFiles.needles`.

### Done when

- `cargo build --release` and `cargo test --release cw::` pass in `bench/`.
- `cargo run --release -- cw gen keys100 /tmp/cw-k --traces 2000`, then `cargo run --release -- cw run /tmp/cw-k --rounds 2 --runs 2`, finish with no `ANSWER MISMATCH`, a summary and verdict per query, and four lines per query in the `.jsonl` file (two rounds × two formats).
- The same for `text_unique --traces 1000` and `giant --traces 100`.

**Commit:** `Wire fmtbench cw run to the case files and the readers`

---

## M1.5: runs and report (orchestrator)

1. On ovh-us, idle (load under 1.5 before each case), build `fmtbench` in release mode.
2. Generate the nine cases with their default sizes, and import `swe` and `wildchat` from `~/bench8/<dataset>` into `~/cw/<case>`.
3. Local verdict runs: `cw run ~/cw/<case> --rounds 25 --runs 2` per case.
4. R2: `fmtbench upload ~/cw/<case> s3://cetra-fmtbench-us/cw/<case> pq_cetra st` (the top-level `cw_*.txt` files go with them), then `FMT_POINT_GAP=262144 FMT_POINT_MAX_RANGE=1048576 fmtbench cw run s3://cetra-fmtbench-us/cw/<case> --rounds 25 --runs 2` for `text_unique`, `b64`, `giant`, `swe` and `wildchat` (the cases whose queries read payloads).
5. Write the verdict per case and query in `strata-readiness.md` § M1, with the losses and their figures, and mark M1 done or failed in the design doc. A loss gets a profile and a fix plan (CW-5) before M2 starts.

## Unverified

- The `ilike` kernel's handling of non-ASCII case (`É`/`é`, `ß`) is whatever arrow 59 does; both formats use it, so the comparison holds either way. M1.3's `intl` test records it.
- Whether `Predicate::Or` of `Contains` terms prunes frames with the n-gram index as a single `Contains` does. Under an `OR`, Strata evaluates each `Contains` over the whole file (`late_contains` takes conjuncts only); M1.5's GETs and bytes show what it costs.
- What the search verdicts cover: five of the six planted needles (`absent`, `rare`, `common`, `bug`, `sql`) have cased characters, so on Strata they take the scan path, a full scan like Parquet's. Only `404`, and `東京` and `🚀` in `intl`, use the n-gram index. The report says which path each search took (the stderr line of `read::run`).
- The 1 MiB-block read cache of spec 03 § 6 is out of M1 (M5).
- How M1 differs from cetra's search (spec 05): M1 searches whole files and returns spans; cetra bounds a search by a time window and runs it as a semi-join from `traces` to `spans`. M3's query shapes add both. The rare needle is planted in every 10,000th non-null input, which is under 0.01% of rows where many rows have no input (`nonllm95`).
