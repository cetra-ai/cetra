# Strata v0 · M4F: scan robustness fixes from the M3F–M4 code review

This is one sub-phase, done by one Haiku agent in one go. It ends with the checks green and one commit.

The review found no wrong result: it ran 840 × 60 random scans and 170 × 50 random lookups and scans against a decoded oracle, all equal. It did find a stack overflow, unbounded work under `limit`, quadratic loops, and memory amplification. This sub-phase fixes them and keeps the reviewer's fuzzers as regression tests.

**Prerequisites:** M4.3 is committed.

**Read first:**
- [`01-m1-container-narrow.md`](01-m1-container-narrow.md): § Conventions.
- [`03-m3-indexes-maps.md`](03-m3-indexes-maps.md): § Global rules.
- [`04-m4-scan.md`](04-m4-scan.md): § Rules for every sub-phase.
- The code named below.

**Edit:**
- `strata/src/{predicate.rs,scan.rs,file.rs,io.rs,lib.rs,payload/mod.rs,payload/read.rs,payload/write.rs}`;
- `strata/tests/{fuzz_scan.rs,fuzz_lookup.rs}` (new);
- `docs/research/file-format/prototype-plan.md` (§ Limits).

GOLDEN must not change: no step touches the bytes the writer produces for valid data.

## Steps

1. **Predicate size limit** (`predicate.rs`).
   - Add `pub(crate) const MAX_PREDICATE_DEPTH: usize = 64;` and `pub(crate) const MAX_PREDICATE_NODES: usize = 4_096;`.
   - `bind` first walks the `Predicate` **iteratively**, with an explicit `Vec` stack of `(node, depth)`. It counts the nodes (each `In` literal counts as one node) and tracks the maximum depth. Above either limit, it returns `Unsupported { what: "predicate too large".to_string() }`, before any recursive function runs.
   - The other recursive functions (`bind_into`, `evaluate`, the walks) stay recursive. The limit bounds their depth.
   - Test: a `Not(Not(..(Cmp)..))` chain 20,000 deep gives `Unsupported` on the default test thread, without overflowing. Build the chain in a loop.

2. **Limit without a predicate** (`scan.rs`). When there is no predicate, build the selection from the candidate ranges only up to `limit` rows. Take rows range by range, and stop as soon as `limit` rows are taken. Do not build the full `Vec<u32>` and truncate it.
   - Test: `limit = Some(3)` with no predicate on `batch(20_000)` gives 3 rows, and the selection never held more. Check this indirectly: the projection wave fetches only the first chunk (`requests == 1` after metadata).

3. **Windowed filter** (`scan.rs`, `filter`).
   - Evaluate the predicate per **window** of at most `FILTER_WINDOW = 65_536` rows inside each candidate range, instead of over the whole range at once.
   - Each window slices the decoded chunk arrays to its rows and concatenates only those pieces. `evaluate` then gets `rows ≤ 65_536`.
   - With `limit = Some(n)`, stop evaluating windows once the selection holds `n` rows.
   - The filter **wave** is unchanged: all surviving predicate chunks are still fetched in one wave. Document on `scan` that with a predicate, `limit` bounds evaluation and output but not the fetched bytes.
   - Hoist the per-range `vec![None; n_dirs]` allocation out of the loop. Reuse one vector and clear it with `fill(None)`.
   - Test: the existing oracle tests cover correctness for short ranges. Add one test where a candidate range is longer than `FILTER_WINDOW`: `batch(150_000)` with a predicate that keeps rows on both sides of row 65,536 and of row 131,072. The result equals the oracle.

4. **Linear pruning and overlap lookups** (`scan.rs`).
   - **Pruning.** Replace the per-pruned-chunk `subtract` calls with one pass. For each predicate column, collect its pruned chunks' row ranges, which are sorted because chunks are in row order. Then call `fn subtract_all(ranges: &[Range<u32>], remove: &[Range<u32>]) -> Vec<Range<u32>>`, a two-pointer merge over both sorted lists.
     - Keep `subtract` only if something else still uses it; otherwise delete it and its test.
     - Unit-test `subtract_all` on the four `subtract` cases, plus a removal list that spans several candidate ranges.
   - **Overlaps.** In `overlapping_chunks`, find the first chunk overlapping each range with `entries.partition_point(|e| e.first_row + e.n_rows <= range.start)`, then walk forward while chunks overlap. Use `u64` for the sums. The existing `overlapping_chunks` test stays green.

5. **Refs-page memory** (`payload/read.rs`, `payload/mod.rs`, `payload/write.rs`).
   - **Move instead of clone.** `ids_of` keeps each decoded page only while its rows are taken. Process pages in ascending index order, and **move** each selected row's `Option<Vec<u32>>` out of the page's vector with `std::mem::take` instead of cloning it. Then drop the page.
   - **Column cap.** Add `pub(crate) const MAX_READ_IDS: usize = 1 << 28;` to `payload/mod.rs`.
     - Every read that expands refs pages (`ids_of`, and hence `read_rows`, `read_payload_column` and `payload_contains`) keeps a running total of the ids it returns. Above `MAX_READ_IDS`, it returns `Corrupt { what: "refs ids" }`.
     - The writer returns `Unsupported { what: "more than 2^28 chunk references in one column".to_string() }` when a column's `chunk_refs` exceeds `MAX_READ_IDS`, so valid files always read back.
   - Test (`payload/read.rs`): `ids_of` over two pages returns the same lists as before. The cap itself is exercised by a unit test of the running-total helper at the limit and one above it, not by building 2^28 ids.

6. **`lookup_scan` warm-only allocation** (`scan.rs`). Build `span_rows` only in the warm branch, where it is used.

7. **Bounded fetch fan-out** (`file.rs`, `fetch`).
   - Replace `try_join_all` over the merged ranges with `futures::stream::iter(ranges).map(|r| self.store.get_range(&self.path, r)).buffered(MAX_CONCURRENT_GETS).try_collect::<Vec<_>>()`. `buffered` keeps the order.
   - Add `pub(crate) const MAX_CONCURRENT_GETS: usize = 64;` to `io.rs`.
   - Counters and waves are unchanged: one `fetch` is still one wave. Tests stay green.

8. **Fuzzers as regression tests** (`tests/fuzz_scan.rs`, `tests/fuzz_lookup.rs`).
   - Copy the reviewer's fuzzers from `/tmp/claude-1000/-home-sebastien-travaux-llm-observer/16446d53-f065-45fb-a416-4a592a2406df/scratchpad/strata/tests/fuzz_scan.rs` and `.../fuzz_lookup.rs`. **Read each file completely before copying it.** If the scratch files are missing, stop and report.
   - Each `#[test]` gets `#[ignore]`. Its seed count is read from the environment variable `STRATA_FUZZ_SEEDS`, defaulting to 40.
   - They use only the crate's public API; fix any use of private items by stopping and reporting.
   - Add both names to the release command below.
   - Also add a `Contains` wave test (`scan.rs`, using `wide_batch(20_000)` with `payload_min_avg: 0`): a `Contains` scan on `blob` with `projection = [uid]` takes at most `1 + 4 + 2` waves (metadata, the up to 4 `Contains` waves, filter and projection). Print the observed count.

9. **Docs** (`prototype-plan.md` § Limits):
   - `MAX_PREDICATE_DEPTH` and `MAX_PREDICATE_NODES`;
   - `MAX_READ_IDS`;
   - `MAX_CONCURRENT_GETS`;
   - "with a predicate, `limit` does not bound the bytes fetched".

## Done when

```bash
cargo +1.99.1 test
cargo +1.99.1 clippy --all-targets -- -D warnings
cargo +1.99.1 fmt --check
cargo +1.99.1 test --release -- --ignored roundtrip_payload roundtrip_light scan_datagen fuzz_scan fuzz_lookup --nocapture
```

**Commit:** `strata: bound predicate size, limit work and refs memory in scans (M4F)`
