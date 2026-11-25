# cetra workload · M5: memory and concurrency

Implementation plan for milestone M5 of [08-cetra-workload-design.md](08-cetra-workload-design.md). Revision 2, after one cold review.

M5 measures what M1 to M4 measure one query at a time, under load: several of cetra's queries at once in one process, and a sequence of queries through cetra's S3 read cache (spec 03 § 6: 1 MiB blocks, LRU). M5.1 and M5.2 are each done by one agent in one go, in order; each ends with the checks green and one commit. M5.3 is the runs and the report, done by the orchestrating session.

**Prerequisites:** M4 is committed (`bench/src/cw/shapes.rs`: `is_shape`, `table_of`, `run`, `ShapeCase`).

**Read first:**
- [08-cetra-workload-design.md](08-cetra-workload-design.md): § Decisions (CW-7, CW-10, CW-12, CW-18), § M5.
- [08-m1-likely-failures.md](08-m1-likely-failures.md): § Conventions, § Build.
- spec 03 § 6 Resource limits (2 GiB per query, 8 GiB in total, CW-10) and § S3 read cache (1 MiB blocks; a range read serves the blocks it holds and fetches the missing ones in one coalesced request; LRU by block).
- `bench/src/cw/{shapes.rs,many.rs,harness.rs}`; `bench/src/counting.rs` (`Counting`, an `ObjectStore` wrapper that counts requests and bytes); `bench/src/main.rs` (`Location`, `location`).

**Conventions:** those of M1. Paths are under `docs/research/file-format/`. Rerun a build once if rustc crashes in thin-LTO codegen.

## What it is, and what it is not

**When M5 is done:**
- `concurrent:<n>` runs `n` of M4's queries at once in one process, on shared opened files; it reports each query's latency and the process's peak memory.
- `cache_mix:<MiB>` runs a fixed sequence of queries twice through an in-process 1 MiB-block LRU cache of `<MiB>` over the store, and reports the second pass's remote requests and bytes.
- M5.3's runs give M5's verdicts, which close the comparison.

**Not in M5:** the disk side of cetra's read cache (its files, CRCs, eviction under free-disk pressure): the cache here keeps blocks in memory, which is enough to compare how each format's reads use 1 MiB blocks.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| CW-7, CW-12 | Strata must be faster at median and p95 and use less memory. |
| CW-10 | The memory budget is cetra's: 2 GiB a query, 8 GiB in all. |
| CW-18 | One fresh process per query, format and round. |

## Versions

No new dependency.

## Phase graph

| Phase | Edit | Depends on |
|---|---|---|
| M5.1 concurrent queries | `bench/src/cw/mod.rs`, `bench/src/cw/load.rs` (new), `bench/src/cw/harness.rs` | M4 |
| M5.2 read cache | `bench/src/cw/load.rs`, `bench/src/cw/cache.rs` (new), `bench/src/cw/mod.rs`, `bench/src/cw/harness.rs` | M5.1 |
| M5.3 runs and report | `strata-readiness.md`, `plans/08-cetra-workload-design.md` (orchestrator) | M5.2 |

---

## M5.1: concurrent queries

**Goal:** `n` of cetra's queries at once in one process, per format.

**Edit:** `bench/src/cw/mod.rs` (`pub mod load;`), `bench/src/cw/load.rs` (new), `bench/src/cw/harness.rs` (route `load::is_load(query)` before the write-like fall-through to `load::load_format`).

### Contracts

```rust
/// The queries `concurrent:<n>` cycles through, in this order.
pub const MIX: [&str; 4] = ["trace_list:cost_total", "facets:7d", "dashboard:30d", "search_1d:bug"];
/// cetra's process-wide query memory (CW-10, spec 03 § 6).
pub const TOTAL_MEMORY: u64 = 8 << 30;

/// `concurrent:<n>` (n from 1 to 64) and, from M5.2, `cache_mix:<MiB>`.
pub(crate) fn is_load(query: &str) -> bool;

/// Runs a load query `runs` times after one warm-up, in this process.
pub(crate) async fn load_format(dir: &str, format: &str, query: &str, runs: usize) -> crate::R<ProcResult>;
```

### Steps

1. **Setup, outside the timed region:** `location(dir)`, `many::list`, the `ShapeCase` (as `harness::shape_format` reads it), and the `spans` and `traces` files opened once each with `many::open` (shared by every query).
2. **One run of `concurrent:<n>`:** the queries `MIX[i % 4]` for `i` in `0..n`. The listing, the opened files and the case are held in `Arc`s; each task gets clones of them and an owned `String` query, so its future is `'static` (`PqFile` and `Arc<StrataFile>` are `Send + Sync`). Each task is a `tokio::spawn` that waits on a shared `tokio::sync::Barrier` of `n`, then calls `shapes::run(&query, files, opened, &case)` (with the files of `shapes::table_of(query)`) and times itself (wall, from just before its call to its return) and returns `(answer, ms)`. The run's latencies are the `n` task times, pushed into `runs_ms` (so the p95 is over queries); `cpu_ms` gets the run's process CPU time divided by `n` per entry. The run's answer is `xxh3_64` of the `n` answers' 16-byte encodings (rows `u64`, hash `u64`) in `i` order; its rows are the sum of the `n` rows.
3. **Warm-up and runs:** one warm-up run, then `loc.store.reset()`, then `runs` measured runs; every run's answer must equal the warm-up's. `gets` and `bytes` after the first measured run; `rss_start` before the setup; `vmhwm` after the runs; `waves`, `bytes_written` and `tail_bytes` are `None`. If `vmhwm` exceeds `TOTAL_MEMORY`, print `over cetra's 8 GiB query memory: <vmhwm>` to stderr (the verdict judges `vmhwm` anyway). The opened files are shared by every query and run, so their metadata caches are warm on both formats, as in a long-running cetra; `cpu_ms` entries are the run's CPU divided by `n` (an average, not a per-query figure); cetra's 2 GiB per-query limit is not checked separately.

### Tests (`load.rs`)

On a project built by a test helper in `load.rs`, `#[cfg(test)] pub(crate) fn test_project(name: &str) -> PathBuf`: a `text_unique` case from `cases::gen_spans("text_unique", Some(200))` and `cases::write_case`, then `project::project_cmd` with `--days 9 --traces-per-day 400 --payloads <that case>`, in a temp dir (M5.2's tests reuse it):
- `concurrent_answers_agree`: `concurrent:4` and `concurrent:8` give equal answers on both formats, and `concurrent:4`'s answer equals the hash of the four `shapes::run` answers run one by one.
- `concurrent_reports_one_latency_per_query`: a `concurrent:8` result with `runs = 2` has 16 `runs_ms` entries.

### Done when

In `bench/`, with `LIBCLANG_PATH=/usr/lib/llvm-18/lib`:
- `cargo build --release`, `cargo test --release cw::` pass.
- On M4.2's `/tmp/cw-p9` (rebuild it as M4.2's Done-when does if it is gone): `target/release/fmtbench cw run /tmp/cw-p9 --queries concurrent:8,concurrent:16 --rounds 2 --runs 2`: no `ANSWER MISMATCH`, a verdict per query.

**Commit:** `Add fmtbench cw concurrent queries`

---

## M5.2: read cache

**Goal:** the hit rate and remote traffic of each format's reads through cetra's 1 MiB-block cache.

**Edit:** `bench/src/cw/cache.rs` (new), `bench/src/cw/mod.rs` (`pub mod cache;`), `bench/src/cw/load.rs`, `bench/src/cw/harness.rs` (`losses` for `cache_mix`).

### Contracts

```rust
// cache.rs
/// Block size of cetra's S3 read cache (spec 03 § 6).
pub const BLOCK: u64 = 1 << 20;

/// An in-memory LRU cache of `BLOCK`-aligned blocks in front of `inner`, as cetra's read cache
/// behaves (spec 03 § 6): a read serves the blocks it holds and fetches the missing ones in one
/// coalesced request (from the first missing block's start to the last missing block's end;
/// the held blocks inside that span are fetched again and count as misses), then keeps every
/// fetched block, evicting the least recently used blocks beyond `budget` bytes. LIST and
/// writes pass through. The object's last block is short.
pub struct BlockCache { /* inner, budget, LRU of (path, block index) -> Bytes, object sizes, stats */ }

impl BlockCache {
    pub fn new(inner: Arc<dyn ObjectStore>, budget: u64) -> Self;
    /// (hits, misses) in blocks since the last reset.
    pub fn stats(&self) -> (u64, u64);
    pub fn reset(&self);
}
// impl ObjectStore for BlockCache: get_opts (ranged and whole), get_ranges (each range as
// get_opts does; the misses of all ranges fetched in one request per range), head, list,
// put/copy/delete pass-through, as counting.rs implements the trait.

// load.rs
/// The sequence `cache_mix` runs twice.
pub const CACHE_MIX: [&str; 6] = ["trace_list:start_time", "facets:7d", "dashboard:30d",
    "search_1d:bug", "lookup_around", "rollup_30d_cold"];
```

**Reads** (`get_opts`):
- `options.head` (what `ObjectStoreExt::head` sends): pass through to the inner store, and remember the returned `ObjectMeta` for the path.
- Any of `if_match`, `if_none_match`, `if_modified_since`, `if_unmodified_since`, `version` set: pass through uncached.
- Otherwise resolve the range against the object's size: `None` → the whole object, `Bounded(a..b)` → `a..min(b, size)`, `Offset(a)` → `a..size`, `Suffix(n)` → `size − min(n, size)..size` (Strata's open reads a suffix and uses `meta.size` and `range.start` of the result). The size comes from the path's remembered `ObjectMeta`, or from one `get_opts` with `head: true` on the inner store, remembered. An empty resolved range, or a start at or past the size, is passed through to the inner store unchanged, so it errors as the inner store does.
- The result: a `GetResult` with `payload` a one-item stream of the assembled `Bytes`, `meta` the remembered `ObjectMeta`, `range` the resolved range, `attributes` default.
- `get_ranges`: each range as a `Bounded` read.

**Rules:**
- Assemble each answer from clones of the held blocks taken under the lock and the freshly fetched blocks, never by reading the LRU again after inserting (a read longer than the budget would lose blocks). Never hold the lock across an `.await`.
- Each block is stored as its own copy (`Bytes::copy_from_slice` of its part of the fetch), so an evicted block frees its memory; the budget counts each block's real length.
- Concurrent reads of the same missing block each fetch it; there is no single-flight. Formats with more concurrent units may fetch a block twice, which counts in their remote traffic, as it would in cetra's cache.
- `stats()`: hits are blocks served from the cache, misses are blocks fetched.
- Remote traffic is the inner `Counting`'s (`loc.store`); it doesn't count the one HEAD per object, the same for both formats.

### Steps

1. `cache.rs`: `BlockCache` as above. The LRU is a `Mutex<(HashMap<(Path, u64), (Bytes, u64 stamp)>, BTreeMap<u64 stamp, (Path, u64)>, u64 bytes)>`, or a simpler structure with the same behavior.
2. `load.rs`, `cache_mix:<MiB>` (`MiB` from 1 to 1,048,576): `location(dir)`, `many::list(&loc, format)` (the format's own listing), `ShapeCase` as `harness::shape_format` reads it, and `many::read_project_case(&loc)` for `lookup_around`. The cached location is `Location { store: Arc::new(Counting::new(Arc::new(BlockCache::new(loc.store.clone(), MiB << 20)))), url: loc.url.clone(), prefix: loc.prefix.clone() }`; readers go through it, and only `loc.store` (the inner `Counting`) is read for remote traffic. One run, `cache_mix_run`: a new cache; then two passes, each opening the `spans` and `traces` files through the cached location (`many::open`, inside the pass, so each pass reads the files' metadata through the cache instead of keeping it in the opened files) and running the six queries of `CACHE_MIX` in order: the `shapes` ones with `shapes::run`, `lookup_around` and `rollup_30d_cold` with `many::run(query, format, &cached, &listing, None, &project_case)` (its per-id times are ignored; one wall time is taken around each call). `loc.store` is reset before each pass.
   ```rust
   /// One `cache_mix` run: the answer of pass 2 and the remote (gets, bytes) of each pass.
   pub(crate) async fn cache_mix_run(dir: &str, format: &str, budget: u64) -> crate::R<(Answer, (u64, u64), (u64, u64), Vec<f64>)>;
   ```
   `load_format` for `cache_mix`: no warm-up (pass 1 warms); each run's answer must equal the first run's; `runs_ms` gets pass 2's six query times; `gets` and `bytes` are pass 2's remote counts of the first run; stderr gets `cache hits <h> misses <m>` of pass 2.
3. `harness.rs` `losses`: for `cache_mix:*` queries, also compare `gets` and `bytes` (Strata must be lower), named `"gets"` and `"bytes"` in the losses list.

### Tests (`cache.rs`, `load.rs`)

- `block_cache_serves_what_it_holds`: over an in-memory store holding a 5 MiB object, a read of `[0, 1.5 MiB)` fetches blocks 0–1 (one inner request), a second read of `[0.5 MiB, 1 MiB)` is a hit with no inner request, and a read of `[1.5 MiB, 3.5 MiB)` fetches blocks 2–3 only (one request from 2 MiB to 4 MiB).
- `block_cache_evicts_least_recent`: budget 2 MiB, reads of blocks 0, 1, 0, 2: block 1 is evicted, block 0 stays.
- `block_cache_answers_like_the_store`: for random ranges over a 10 MiB object, the cache returns the same bytes as the inner store.
- `cache_mix_agrees_across_formats`: on `test_project`, `cache_mix_run` with a 64 MiB budget gives equal answers on both formats; on each format pass 1 makes at least one remote request and pass 2 at most as many as pass 1.

### Done when

- `cargo build --release`, `cargo test --release cw::` pass.
- `target/release/fmtbench cw run /tmp/cw-p9 --queries cache_mix:64,cache_mix:256 --rounds 2 --runs 1`: no `ANSWER MISMATCH`, a verdict per query.

**Commit:** `Add fmtbench cw read-cache mix`

---

## M5.3: runs and report (orchestrator)

On ovh-us, idle, after M4's runs, on `~/cw/p30-small` and the verdict project:
1. `cw run <dir> --queries concurrent:8,concurrent:16 --rounds 26 --runs 2`, locally and on R2.
2. On R2 only: `cw run s3://… --queries cache_mix:<10% of the Parquet project's bytes, in MiB>,cache_mix:<25%> --rounds 10 --runs 1`, the same budgets for both formats, computed from `cw layout`'s Parquet totals of `spans` and `traces`.
3. Write M5's verdicts in `strata-readiness.md` § M5 and the overall verdict of the comparison, and mark M5 in the design doc.

## Unverified

- Whether 16 concurrent queries of the verdict project fit in 8 GiB on either format: the run reports it.
- How cetra's disk cache's per-block CRC and file overhead would change the picture: not modelled (the same for both formats).
