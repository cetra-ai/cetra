# Strata v0 · M7: real data, bench variants and runs

Implementation plan for phases 9a, 9b and 9c of [the prototype design](../prototype-plan.md), § Phases, § Benchmark protocol and § Go / no-go criteria. Revision 3, after two cold reviews. The second review compiled the new `Counting`, `AmazonS3Builder` and `pqfast` code in a probe crate.

M7.1 to M7.3 are each done by one Haiku agent in one go, in order. Each ends with the checks green and one commit. M7.4 is the runs and the report, done by the orchestrating session.

**Prerequisites:** M6.2 is committed.

**Read first:**
- [`01-m1-container-narrow.md`](01-m1-container-narrow.md): § Conventions. The bench and testdata crates are research tools: `unwrap` and `expect` are allowed there, but not in the `strata` crate, which M7 does not change.
- [`../prototype-plan.md`](../prototype-plan.md): § Phases 9a–9c, § Benchmark protocol, § Go / no-go criteria.
- `docs/research/file-format/bench/src/{main.rs,counting.rs,profile.rs,pqfast.rs}`. `pqfast.rs` is a verified engine-free Parquet lookup.
- `docs/research/file-format/testdata/src/{datagen,swe}.rs`.
- `crates/core/src/{span.rs,ids.rs,enums.rs,schema.rs,batch.rs}`:
  - `Span` has no `duration_ns` and no `model` field, and `status` is a `SpanStatus`;
  - `PayloadFormat` has `Messages` and `Text`;
  - `TraceId`/`SpanId::from_bytes` return `Result` and reject all-zero ids.
- The `strata` public API: `StrataWriter` (whose `finish()` is sync and returns `(Vec<u8>, FileSummary)`), `StrataFile::open`, `StrataTable::new`, `trace_detail`, `trace_list`, `merge`, `IoStats`, `Predicate`, `Lit`, `CmpOp`.

| Sub-phase | Content |
|---|---|
| M7.1 | `swe::spans`: SWE-agent trajectories as full spans; `gen --swe`; needles; a tiny test shard |
| M7.2a | bench plumbing: store, location and context helpers, S3 switch, `Format`, `open_strata`, `gen` writing `st`, payload table, `profile` with `st` |
| M7.2b | query runners: `st`, `st_fast`, `pq_fast`; warm-up; means and waves; list ordering; substring needles |
| M7.3 | `trace_lookup_cold`, the `merge` command, CPU time and load average |
| M7.4 | runs on `ubuntu-vm` (local disk and S3 with latency), README § Results, go/no-go (orchestrator) |

---

## M7.1: `swe-spans` dataset

**Goal:** turn the SWE-agent trajectories into spans that satisfy every `Span` invariant, so that every bench query runs on real payloads.

**Edit:** `testdata/{Cargo.toml,src/swe.rs,src/datagen.rs}`, `bench/src/main.rs`.

### Steps

1. **`swe::spans`** (`testdata/src/swe.rs`):

   ```rust
   /// Spans built from SWE-agent trajectories: one trace per trajectory, at most
   /// `max_trajectories`, read from the given shards in order.
   pub fn spans(paths: &[PathBuf], max_trajectories: usize, world: &datagen::World, rng: &mut datagen::Rng)
       -> Result<Vec<Span>, Box<dyn std::error::Error + Send + Sync>>;
   ```

   **Reading.**
   - Do **not** refactor `payloads`: its loop is interleaved with chunking and the needle.
   - `spans` reads the `trajectory` column with the same projection, batch size and per-message parsing (role, text, running system prompt, history JSON). Duplicate those ~20 lines.
   - The real-shard test below checks the two against each other.

   **Steps of a trajectory.** Walk its messages in order. Each `ai`/`assistant` message, and each `user` message after the first `user` message, is a **step** `k = 0, 1, …`. The root is not counted.

   **Templates,** once per trajectory, with a fresh empty `tmp` and the shared `rng`, which advances:
   - call `world.trace(rng, &mut tmp)`;
   - the **root template** is `tmp[0]`, which is always the `Agent` root;
   - the **LLM template** is the first span of `tmp` with the LLM kind (check the `SpanKind` variant name). `World::trace` always makes at least one;
   - the **tool template** is a clone of the LLM template with:
     - kind `Tool` and name `"tool"`;
     - empty attributes;
     - all of these set to `None`:
       - `provider`, `request_model`, `response_model`, `operation`;
       - every token field;
       - every cost field, and `cost_source`;
       - `ttft_ns` and `request_params`;
       - `input`, `output`, `input_format`, `output_format`, `input_preview` and `output_preview`;
       - `system_instructions` and `tool_definitions`.

     Use the field names that exist in `Span`; skip any that does not.

   **On every clone:**
   - set `status = SpanStatus::Unset`, `status_message = None` and `events = vec![]`;
   - set `cache_read_tokens`, `cache_write_tokens` and `reasoning_tokens` to `None` where they exist.

   **Ids.**
   - `trace_id = TraceId::from_bytes(xxh3_128(&(index as u64).to_le_bytes()).to_be_bytes()).expect("non-zero id")`, where `index` counts trajectories from 0.
   - Span `j` (root `j = 0`, step `k` → `j = k + 1`) has `SpanId::from_bytes((((index as u64) + 1) << 20 | j as u64).to_be_bytes()).expect("non-zero id")`.
   - Add `xxhash-rust = { version = "0.8", features = ["xxh3"] }` to `testdata/Cargo.toml`.

   **Times.**
   - `start` is the root template's `start_time`.
   - Step `k` starts at `start + (k + 1) × 2 × NS` and ends `1 × NS` later.
   - The root spans from `start` to the last step's end, or to `start + NS` when there is no step.

   **Previews.** Make datagen's `fn preview` `pub` and use it for every preview.

   **Root span.** A clone of the root template with the new ids and times, plus:
   - `input` = `[{"role":"user","parts":[{"type":"text","content":<first user text>}]}]`;
   - `output` = the same shape as `payloads`' output JSON for the last `ai`/`assistant` text.

   Both are `PayloadFormat::Messages`, with previews. A missing message makes that field, its format and its preview `None`. `input` is present exactly when `input_format` is, and the same goes for `output`.

   **LLM spans,** one per `ai`/`assistant` step. A clone of the LLM template with:
   - the new ids, the root as parent, and the step times;
   - `input`, `output` and `system_instructions` built exactly as `payloads` builds them;
   - `input` and `output` with format `PayloadFormat::Messages` and previews; `tool_definitions: None`;
   - **costs:**
     - `in = (input.len() + system_instructions.len()) / 4` and `out = output.len() / 4`;
     - input tokens `in`, output tokens `out`, total `in + out`;
     - `cost_input = in × 2.5e-6`, `cost_output = out × 1e-5`, and `cost_total` their sum;
     - the `cost_source` variant datagen uses for computed costs;
     - `ttft_ns = Some(NS / 5)`.

   **Tool spans,** one per `user` step. A clone of the tool template with:
   - the new ids, the root as parent, and the step times;
   - `output = Some(text)`, `output_format = Some(PayloadFormat::Text)` and a preview;
   - no input.

   **Output.** All spans, in trajectory order.

   **Memory.** Both shards hold about 2 GB of strings, so `gen --swe` peaks around 6 GB. That is fine on `ubuntu-vm` (62 GB).

2. **`gen --swe`** (`bench/src/main.rs`).
   - **Syntax.** `fmtbench gen <dir> <traces> <files> [--swe <max_trajectories> <shard>...]`:
     - `args[5] == "--swe"`;
     - `args[6]` is the maximum number of trajectories;
     - `args[7..]` are the shards.
   - Update the usage string.
   - **Spans.** With `--swe`, the spans come from `swe::spans(..)` with `World::new(7)` and `Rng::new(42)`. Print "traces argument ignored", and skip the needle planting. The trace-id sample, the slicing and every variant are unchanged.
   - **Needles.** Write `needles.txt`: 20 needles, one per line, hex-encoded. Use `Rng::new(77)`:
     - Pick a random LLM span with a non-null `output`. Take a substring at `char` boundaries, 8 to 32 bytes long, starting near the middle. Reject it if it contains `%`, `_`, `\`, `'`, `\n` or `\r`.
     - Count the spans whose `input` contains it, with `memchr::memmem::Finder`, stopping above 1,000. Add `memchr = "2"` to bench if it is missing.
     - Keep it when it has 1 to 1,000 hits. Stop at 20 needles, or after 2,000 attempts.

     Print each needle's hit count.
   - **Empty queries.** On swe data, `substring_input`, `user_filter` and `metadata_filter` return 0 rows. That is expected.

3. **Tests** (`testdata/src/swe.rs`).
   - **Tiny shard** (a normal test). Write, with `ArrowWriter` into `std::env::temp_dir().join(format!("swe-test-{}", std::process::id()))`, a Parquet file with one column `trajectory: List<Struct{cutoff_date: Utf8, mask: Boolean, role: Utf8, system_prompt: Utf8, text: Utf8}>`:
     - all struct children and the list item are nullable;
     - every `mask` has a value.

     It holds 2 trajectories: one with system, user, ai, user, ai, user messages, and one with system and user only. Run `spans` on it, then assert:
     - the root comes first in each trace;
     - the LLM count equals the `ai` messages, and the tool count equals the `user` messages minus 1;
     - every non-root span's parent is the root;
     - all ids are distinct;
     - `spans_to_record_batch(&spans)` succeeds;
     - `input.is_some() == input_format.is_some()` and `output.is_some() == output_format.is_some()` for every span.

     Remove the directory at the end.
   - **Real shards** (`#[ignore]`, runs only when `FMT_SWE_SHARDS` holds a comma-separated list of paths).
     - Call `p = payloads(&paths, 512 << 20, 4)`. Then `spans(&paths, p.trajectories, ..)` must have exactly `p.spans` LLM spans.
     - Their summed `input`, `output` and `system_instructions` byte lengths must equal the sums over `p.chunks`.
     - Assert `p.trajectories == 1674` and `p.spans == 47_198`.

### Done when

```bash
cd docs/research/file-format/testdata && cargo +1.99.1 build && cargo +1.99.1 test
cd ../bench && cargo +1.99.1 build --release
```

**Commit:** `fmtbench: add the swe-spans dataset (M7.1)`

---

## M7.2a: bench plumbing

**Goal:** every bench command reads its dataset through one store, which is local or S3. Strata files are written and openable. `profile` covers `st`.

**Edit:** `bench/{Cargo.toml,src/main.rs,src/counting.rs,src/profile.rs}`.

### Steps

1. **Dependencies.**
   - Add `strata = { path = "../strata" }`.
   - Add the `aws` feature to `object_store`.
   - Run `cargo +1.99.1 tree --prefix none | grep -E '^(arrow|parquet|datafusion|object_store) ' | sed 's/ (\*)$//' | sort -u`. It must print one version per crate; if it prints more, stop and report.

2. **`Counting`** (`counting.rs`). It wraps `inner: Arc<dyn ObjectStore>`, with `Counting::new(inner)`, forwarding every method as today.

3. **Location and context** (`main.rs`):

   ```rust
   struct Location { store: Arc<Counting>, url: url::Url, prefix: object_store::path::Path }
   fn location(dir: &str) -> R<Location>;
   fn new_ctx(loc: &Location, partitions: Option<usize>) -> R<SessionContext>;
   async fn read_text(loc: &Location, name: &str) -> R<Option<String>>;
   async fn has_files(loc: &Location, variant: &str) -> R<bool>;
   async fn open_strata(loc: &Location, variant: &str) -> R<Vec<Arc<StrataFile>>>;
   ```

   - **`location`:**
     - **local** (`dir` has no scheme): canonicalize it. `url` is `file:///` (the store root), `prefix` is `Path::from_filesystem_path(&abs)?` (the absolute directory, without a leading slash), and the inner store is `LocalFileSystem::new()`.
     - **S3** (`dir` is `s3://bucket/key`): the inner store is `AmazonS3Builder::from_env().with_bucket_name(bucket).with_virtual_hosted_style_request(false).build()?`. `url` is `s3://bucket/` and `prefix` is `key`. Required environment variables: `AWS_ENDPOINT`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_DEFAULT_REGION` and `AWS_ALLOW_HTTP=true`. Document them in the usage text.
   - **`new_ctx`** builds the `SessionConfig` that `query` uses today (the Parquet pushdown settings), plus `with_target_partitions(n)` when `partitions` is `Some(n)`. It calls `ctx.register_object_store(&loc.url, loc.store.clone())`. **Every** `SessionContext` in the bench is made with it.
   - **`read_text`** reads `prefix.join(name)` with `use object_store::ObjectStoreExt;` and `store.get(..).await?.bytes().await?`. A not-found error gives `None`.
   - **`has_files`** is true when `store.list(Some(&prefix.join(variant)))` yields at least one object.
   - **`open_strata`** lists `prefix.join(variant)` and keeps the `.strata` objects sorted by path. It opens them concurrently with `try_join_all` of `StrataFile::open(loc.store.clone() as Arc<dyn ObjectStore>, path, None, &Config::default())`, then maps each with `Arc::new`.
   - Use `Path::join`, not the deprecated `child`. It takes `self` by value: `loc.prefix.clone().join(..)`.
   - Every listing URL is `loc.url` joined with `prefix/variant/`; this is right for both local (`file:///` + absolute path) and S3 (`s3://bucket/` + key).

4. **`Format` and `table`.**
   - `enum Format { Parquet, Vortex, Strata }`, with `Format::of(variant)` from the directory variant's prefix (`pq`, `vx`, `st`). It applies only to directory variants; `st_fast` and `pq_fast` are runner kinds (M7.2b).
   - `async fn table(ctx: &SessionContext, loc: &Location, variant: &str) -> R<Vec<Arc<StrataFile>>>` registers `t`:
     - Parquet and Vortex as today, with the listing URL `loc.url` joined with `prefix/variant/`;
     - Strata with `StrataTable::new(open_strata(..).await?)`.

     It returns the Strata files, or an empty `Vec` for the other formats.

5. **No local reads in `query` and `profile`.**
   - `trace_ids.txt` is read with `read_text`.
   - Variants are skipped when `!has_files(..)`.
   - `profile()` takes `&Location` instead of `&Path`, and its contexts come from `new_ctx`.
   - The `canonicalize` calls move into `location`.
   - Delete `profile.rs`'s `Direct` reader and its `FMT_DIRECT` block, which read local files. `pq_fast` (M7.2b) replaces it as the engine-free Parquet floor. `unhex` stays.

6. **`gen` writes `st`** (gen stays local-only).
   - After the Vortex variants, `create_dir_all(<dir>/st)`. Then, for each slice:
     - `StrataWriter::new(schema, &["trace_id", "start_time"], Config::default())`;
     - write `b.slice(off, 8192.min(..))` pieces;
     - `let (bytes, summary) = w.finish()?;`;
     - `std::fs::write(<dir>/st/{i:03}.strata, &bytes)`.

     All of this is inside the timed section.
   - Print the size and throughput line under `st`. All writers are single-threaded: these are the G8 figures.
   - **Payload table.** For `input`, `output`, `system_instructions` and `tool_definitions`, print:
     - the `st` bytes, summed over the summaries' `columns[i].bytes`;
     - when `columns[i].payload` is `Some`, the summed `frames` and `frames_without_filter` (`u32`) and the percentage of frames with a filter; `-` when it is `None`;
     - the column bytes for each of the three Parquet variants. Generalise `column_sizes_parquet(dir, variant) -> Vec<(String, u64)>`, and keep its existing `pq_tuned` printout.

7. **`profile` with `st`.**
   - Add `st` to the default `FMT_VARIANTS`.
   - `StrataExec` exposes `output_rows`, `elapsed_compute` and the `strata_*` counters, not the Parquet metric names `profile` reads. Its G3 execute time is therefore the stopwatch execute time `profile` already measures. Print `-` for the missing metrics.
   - Change `profile.rs`'s list query to `ORDER BY start_time DESC, trace_id DESC, span_id DESC LIMIT 50`.

### Done when

```bash
cd docs/research/file-format/bench && cargo +1.99.1 build --release
D=/tmp/claude-1000/-home-sebastien-travaux-llm-observer/16446d53-f065-45fb-a416-4a592a2406df/scratchpad/m7
./target/release/fmtbench gen $D 2000 4
./target/release/fmtbench query $D 3
FMT_VARIANTS=pq_default,st ./target/release/fmtbench profile $D 3
```

`query` runs `st` through `table()` like the other directory variants. No line prints `ERROR`, and every query's `st` row count equals `pq_default`'s. Include the outputs in the report, then remove `$D`.

**Commit:** `fmtbench: add S3 locations, Strata files and the payload table (M7.2a)`

---

## M7.2b: query runners

**Goal:** `query` runs the engine-free variants beside the SQL ones. It prints per-lookup means with metadata cached, waves, and the G7 substring figures.

**Edit:** `bench/src/{main.rs,profile.rs,pqfast.rs}`.

### Steps

1. **Shared column lists.** Define `pub const LIGHT_COLS: [&str; 9]` and `pub const LIST_COLS: [&str; 10]` once, in `main.rs`, from the current light-lookup and list query texts. Build those SQL texts, `profile.rs`'s, and the fast-path projections from them.

2. **Ids.** Make `profile.rs`'s `unhex` `pub(crate)`. Convert ids with `let k: [u8; 16] = unhex(id).try_into().map_err(|_| "bad trace id")?;`.

3. **`pqfast.rs`.** Add `mod pqfast;`. Do not change its logic. `open` takes `loc.store.clone()` and each file's `Path`, from listing `prefix.join(variant)` for `.parquet` objects.

4. **Runners:**

   ```rust
   enum Runner { Sql(SessionContext), StFast(Vec<Arc<StrataFile>>), PqFast(Vec<pqfast::PqFile>) }
   /// Runs one query (all its SQL texts or lookups) and returns the row count; None when the
   /// runner does not support the query.
   async fn run_query(r: &Runner, q: &Query, ids: &[[u8; 16]]) -> R<Option<usize>>;
   ```

   - **`Query`** is the existing `(name, sql)` pair, with a kind: `Sql`, `TraceFull`, `TraceLight` or `ListLlm`.
   - **`Sql`** runs as today.
   - **`StFast`** supports only:
     - `TraceFull`: `trace_detail(files, id, &all names)` per id;
     - `TraceLight`: the same with `LIGHT_COLS`;
     - `ListLlm`: `trace_list(files, (t0 + 14 × h)..(t0 + 15 × h), Some(Predicate::Cmp(files[0].schema().index_of("span_kind")?, CmpOp::Eq, Lit::Utf8("llm".into()))), 50, &LIST_COLS)`, with `t0 = datagen::DAY_START` and `h = 3_600 × datagen::NS`.

     The `st` files are sorted by trace id, so `trace_list` visits every chunk in the window. That is expected.
   - **`PqFast`** supports only `TraceFull` and `TraceLight`: per id, `lookup` on every file. The projection is the root indexes `f.meta.schema().index_of(name)`, sorted ascending.
   - **Variants.** The directory variants run with `Sql(new_ctx(..))` after `table()`. Then `st_fast` runs with `StFast(open_strata(loc, "st"))`. Then `pq_fast/<v>` runs, for each Parquet variant, with `PqFast(<files opened concurrently>)`. Unsupported queries print `n/a`.

5. **Warm-up and means.**
   - Before a query's measured runs, run it once and discard the result. Lazy metadata loads (Strata block tables and dictionaries, Parquet footers and page indexes) then fall outside the measurement.
   - I/O figures come from the **first measured run**: `Counting` is reset before it, and for Strata files `reset_io_stats` is called before it too.
   - For `TraceFull` and `TraceLight`, print requests, KiB and waves **divided by the number of ids**, for every variant.

6. **Waves** (`st`, `st_fast` and `pq_fast`; others print `-`). Print two figures:
   - `waves_max`, the max over files of `io_stats().waves`, valid when the files are read concurrently: lookups, and `st` with at least as many partitions as files;
   - `waves_sum`, the sum over files, valid for `trace_list`, which visits chunks one after another.

   Label them in the header.

7. **List ordering.** In `main.rs`, `list_llm_1h` becomes `ORDER BY start_time DESC, trace_id DESC, span_id DESC LIMIT 50`.

8. **Substring needles (G7).** If `read_text(loc, "needles.txt")` gives needles, run `substring_needles` for every directory variant. Each needle's query is `SELECT count(*) FROM t WHERE input LIKE '%<needle>%'`.
   - **Time.** In one `new_ctx(loc, Some(1))` context with the table registered, run all needles once to warm up. Then, `FMT_NEEDLE_RUNS` times (defaulting to the run count), time all needles. Print the minimum over runs of (time / needle count).
   - **Cold bytes.** For each needle, take a fresh `new_ctx(loc, Some(1))` and `table()`, `Counting::reset`, run the query, and read the bytes. Print the mean over needles.

### Done when

The M7.2a commands pass, plus:
- `st_fast` row counts equal `pq_default`'s for its queries: for `trace_*` the sum over ids, for `list_llm_1h` 50;
- `pq_fast` matches the trace sums.

Include the `query` output in the report.

**Commit:** `fmtbench: add fast-path runners, warm means and waves (M7.2b)`

---

## M7.3: cold lookups, merge, CPU time

**Goal:** `fmtbench` covers G1b (cold lookups), G9 (merge), and the protocol's CPU time and load average.

**Edit:** `bench/src/{main.rs,profile.rs}`.

### Steps

1. **`trace_lookup_cold`.** A new query over the first 20 ids. For each id, `Counting::reset`, then everything fresh:

   | Variant | Per id |
   |---|---|
   | `st` | `new_ctx`, `table()` (re-opens the files concurrently), `SELECT * FROM t WHERE trace_id = X'..'` |
   | `st_fast` | `open_strata` again, then `trace_detail` with all columns |
   | Parquet and Vortex | `new_ctx`, `table()`, the same SQL |
   | `pq_fast/<v>` | open every file again concurrently, then `lookup` |

   - **Cold here** means no in-process cache: DataFusion 55.2 keeps its file-metadata and listing caches per `RuntimeEnv`, and each new context has its own. On local disk the OS page cache stays warm; say so in a comment and in the output.
   - **Bytes and requests** come from `Counting`, read after the lookup, so they include the open. Requests exclude HEAD and LIST, which `Counting` does not count: print a note.
   - **Waves** for `st` and `st_fast`: `open_waves + max over files of io_stats().waves`, where `open_waves = 1` if the requests during the open equal the file count, and 2 otherwise. Measure that count by reading `Counting` right after the open.
   - **Baseline.** Print a `SELECT 1` time in a fresh `new_ctx`.
   - Print the mean per id of time, bytes, requests and waves.

2. **`fmtbench merge <dir>`** (G9). It is **local-only**: canonicalize `dir`, and reject `s3://` with an error.
   - **Strata.** Open the 4 `st` files with `open_strata`, then time `strata::merge(&files, &["trace_id", "start_time"], Config::default()).await` plus `std::fs::write(<dir>/merged.strata, ..)`. Print a note that the object store's I/O runs on blocking threads.
   - **Parquet.** Time reading the 4 `pq_default` files fully with the sync `ParquetRecordBatchReaderBuilder` (as `payload_cmd` does), then `concat_batches`, `lexsort_to_indices` on `(trace_id, start_time)`, `take_record_batch`, and writing one file with `ArrowWriter` and `parquet_props("pq_default", ..)` to `<dir>/merged.parquet`.
   - **Overflow.** If `concat_batches` or `take` fails with an offset overflow, print the error and continue.
   - Print both times and the ratio.

3. **CPU time and load.**
   - `query` and `merge` print the process CPU time (the `cpu-time` crate is already a dependency) next to wall time: the median per query.
   - They print the 1-minute load average from `/proc/loadavg` at the start and at the end.

### Done when

The M7.2a commands pass, plus:

```bash
./target/release/fmtbench gen $D 2000 4 && ./target/release/fmtbench query $D 3 trace_lookup_cold && ./target/release/fmtbench merge $D
```

The cold rows' row sums equal `pq_default`'s. Include the output, then remove `$D`.

**Commit:** `fmtbench: add cold lookups, the merge command, CPU time and load average (M7.3)`

---

## M7.4: runs and report (orchestrator)

1. **Data.**
   - `payload`: 60,000 traces, 4 files.
   - `light`: 300,000 traces, `FMT_LIGHT=1`, 4 files.
   - `swe-spans`: both shards, `--swe 1674`, 4 files. This needs the shards, downloaded with the user's approval.
2. **Where.**
   - On `ubuntu-vm` (see `test-vm.md`): rsync the research directories and build there.
   - Run only while the VM is idle; the bench logs the load.
3. **Local disk.**
   - `query <dir> 30`, using `FMT_NEEDLE_RUNS=5` for swe.
   - `profile` for G3.
   - `merge` for `payload`.
4. **S3 with latency.**
   - Start Versity Gateway v1.11.0 behind toxiproxy 2.13.0, adding 20 ms ± 5 ms per request (settings in `docs/research/storage/benchmarks.md`).
   - Upload each dataset directory to the gateway.
   - Run `query s3://bucket/<dataset> 30` with the `AWS_*` variables of M7.2a pointing at the toxiproxy listener.
5. **Report.** README § Results: one table per criterion G1a–G9, the go/no-go verdict per § Outcome, and the deviations.
