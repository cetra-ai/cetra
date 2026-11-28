# Strata v0 · M6: merge

Implementation plan for phase 8 of [the prototype design](../prototype-plan.md), § S10. Revision 3, after two cold reviews. The second review prototyped both sub-phases; its measured numbers are quoted below. M6 starts from a clean `cargo clippy`.

There are two sub-phases. Each is done by one Haiku agent in one go, in order, and ends with the checks green and one commit.

**Prerequisites:** M5.2 is committed.

**Read first:**
- [`01-m1-container-narrow.md`](01-m1-container-narrow.md): § Versions and § Conventions.
- [`03-m3-indexes-maps.md`](03-m3-indexes-maps.md): § Global rules.
- [`04-m4-scan.md`](04-m4-scan.md): § Rules for every sub-phase.
- [`../prototype-plan.md`](../prototype-plan.md): § S4 (payload layer) and § S10.
- The code in `docs/research/file-format/strata/src/`:
  - `writer.rs`;
  - `payload/{mod,write,frames,read}.rs`;
  - `file.rs` (`fetch`);
  - `reader.rs`;
  - `segment.rs`;
  - `testgen.rs`.

| Sub-phase | Content |
|---|---|
| M6.1 | `merge`: k-way merge of the sort key, then each column decoded, interleaved and re-encoded by the writer's own code; memory bound tested |
| M6.2 | payload frame copy: input frames whose chunks are all new are copied byte for byte |

**What merge re-decides.** The output is written by the same code as `StrataWriter::finish`, from the merged column arrays. Every per-column decision of design S10 step 2 is therefore taken again on the output data: payload or narrow, dictionaries, map promotion and key filters.

**Code rules.** In non-test code:
- no `v[i]`, `.unwrap()` or `.expect()`: use `.get()` and `?`;
- new test modules carry the same `#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]` block as `reader.rs`'s tests.

---

## M6.1: merge by re-encoding

**Goal:** `merge` writes one file holding every row of its inputs, sorted by the sort key. Its peak memory is about twice the largest decoded column plus the row-source map.

**Edit:**
- `strata/src/{writer.rs,merge.rs (new),lib.rs}`;
- `strata/tests/merge_memory.rs` (new);
- `docs/research/file-format/prototype-plan.md` § S10.

### Steps

1. **Split the writer** (`writer.rs`). Move the body of `finish` into a free function that takes its columns from a closure:

   ```rust
   /// Writes a file of `rows` rows whose column `i` is `column(i).await`. The rows must already
   /// be sorted by `sort_key` (schema indexes); this is not checked here.
   pub(crate) async fn write_file<F, Fut>(schema: &SchemaRef, sort_key: &[usize], config: &Config,
                                          rows: u64, mut column: F) -> Result<(Vec<u8>, FileSummary)>
   where F: FnMut(usize) -> Fut, Fut: Future<Output = Result<ArrayRef>>;
   ```

   - **`write_narrow`** becomes a free function `fn write_narrow(config: &Config, array, column_id, codec, out, pages)`, with the same other parameters. That is 6 parameters, under clippy's limit, so no wrapper struct is needed.
   - **`finish`** becomes `futures::executor::block_on(write_file(&self.schema, &self.sort_key, &self.config, self.rows, |i| std::future::ready(self.column(i))))`. Each future is immediately ready, so `block_on` is safe even inside a tokio test.
   - Each column is requested exactly once, in schema order, and dropped before the next one is requested.
   - **Golden check.** GOLDEN must not change, and `golden_output_hash` must pass after this step, before step 2.
   - **Leftovers from the move.**
     - A multi-line `self\n.config\n.key_filter_columns` (around `writer.rs:291`) is easy to miss with a line-based replace.
     - After renaming `self.x` to `x`, run `cargo clippy`: it flags `rows: rows` (`redundant_field_names`) and needless borrows such as `&config` and `Arc::clone(&schema)` when `schema` is already a `&SchemaRef`.

2. **Sort-key validation** (`writer.rs`). Move the sort-key checks of `StrataWriter::new` into:

   ```rust
   pub(crate) fn sort_key_indexes(schema: &Schema, sort_key: &[&str]) -> Result<(Vec<usize>, RowConverter)>;
   ```

   `new` calls it. Its errors are unchanged.

3. **`merge`** (`merge.rs`):

   ```rust
   /// Merges `inputs` into one file sorted by `sort_key`. Rows with equal keys keep the order of
   /// their input, then their row. Returns the file bytes and its summary.
   pub async fn merge(inputs: &[Arc<StrataFile>], sort_key: &[&str], config: Config) -> Result<(Vec<u8>, FileSummary)>;
   ```

   `lib.rs` adds `mod merge;` and `pub use merge::merge;`.

   1. **Checks.**
      - At least 1 input; otherwise `Schema { reason: "no input" }`.
      - Every input's schema equals the first one's; otherwise `Schema { reason: "inputs have different schemas" }`.
      - Call `StrataWriter::new(schema, sort_key, config.clone())` and drop the result: this runs every config and sort-key check.
      - Every input's sort-key column must be `Some(index of sort_key[0])`; otherwise `Schema { reason: "input sorted by another key" }`. Read it with the accessor `file.rs` already has: M4.3 added `pub(crate) fn sort_key_column()`, and M5.1 may have added `pub fn sort_key()`. Check with `grep -n "fn sort_key" src/file.rs`, use one of them, and do not add a third.
      - The total row count must be `≤ MAX_ROWS`; otherwise `TooManyRows`.
   2. **Kept inputs.** Let `kept` be the inputs with `row_count() > 0`, in order. Positions in `kept` are the input indexes used from here on.

      If `kept` is empty, write an empty file: `write_file` with `rows = 0`, where every column is `new_empty_array(field type)`.
   3. **Row-source map.**
      - For each kept input, `read_column` every sort-key column, then `converter.convert_columns(&arrays)?`. Use the same `RowConverter` for every input. This gives one `Rows` per input.
      - **Heap.** Seed a `BinaryHeap<Reverse<(OwnedRow, usize, usize)>>` with `(rows[i].row(0).owned(), i, 0)` for each input.
      - **Pop.** Pop `(key, i, r)`, push `(i, r)` to `sources: Vec<(usize, usize)>`, then push `rows[i].row(r + 1).owned()` if `r + 1 < rows[i].num_rows()`. The guard is required: `Rows::row` panics out of range.
      - **No pre-sizing.** Do not call `Vec::with_capacity` with the files' claimed row counts. Grow `sources` by pushing, so a crafted file cannot trigger a huge allocation. After the loop, check `sources.len() as u64 == total rows`; otherwise `Corrupt { what: "merge row count", offset: 0 }`.
      - **Sortedness.** Before pushing, compare `rows[i].row(r + 1)` with `rows[i].row(r)`. If it is smaller, return `Unsorted { row: sources.len() as u64 }`, the output position, not the input row. The heap holds at most one row per input.
      - **Tie-break.** `OwnedRow` has `Ord`; the tuple order gives the tie-break.
      - **Memory.** `sources` costs 16 bytes per row; document that in S10. Drop the `Rows` and arrays before step 4.
   4. **Columns.** Call `write_file` with this closure, which compiles: the `FnMut` closure must not lend borrowed captures to the future it returns.

      ```rust
      let live: &[Arc<StrataFile>] = &kept;
      let sources = Arc::new(sources);
      write_file(&schema, &key_indexes, &config, rows, move |i| {
          let sources = Arc::clone(&sources);
          async move {
              let arrays = try_join_all(live.iter().map(|f| read_column(f, i))).await?;
              let refs: Vec<&dyn Array> = arrays.iter().map(|a| a.as_ref()).collect();
              Ok(interleave(&refs, &sources)?)
          }
      }).await
      ```

      `kept` is a `Vec<Arc<StrataFile>>` that lives in `merge` for the whole call.

4. **Design doc** (§ S10) and the `lib.rs` "current limits" doc, which gains a merge line:
   - Steps 2 and 4 of S10 change: the decisions are taken again from the merged arrays, not from "summed input statistics" or "the union of the dictionaries".
   - `merge(&[Arc<StrataFile>], &[&str], Config)` returns `(Vec<u8>, FileSummary)` like the writer, instead of writing to `impl Write`.
   - The row-source map costs 16 bytes per row, and there is no 255-input limit.
   - Inputs must be sorted by the merge key; this is checked.
   - Empty inputs are skipped.
   - M6.1 re-encodes every column; M6.2 adds the frame copy.

### Tests

**Unit tests** (`merge.rs`). They use `crate::testgen` and their own helper `async fn open(bytes: Vec<u8>) -> Arc<StrataFile>`, which calls `InMemory::put` then `StrataFile::open(store, path, None, &Config::default())`.

- **Equivalence.**
  - Split `wide_batch(8_000)` into 4 files of rows `i % 4 == j` (with `take_record_batch`).
  - Merge them with `["trace_id", "start_time"]`.
  - Every column of the output (`read_column`) equals the same column of a file written from `wide_batch(8_000)` in one go. Every `(trace_id, start_time)` key is unique, because `start_time` increases.
- **Golden** (default config only).
  - The merged bytes equal the one-go file's bytes.
  - If they differ, find the first column whose `FileSummary.columns[i].bytes` differ, and stop and report it. Do not weaken the test.
- **Empty inputs.** `merge([A, empty])` equals `merge([A])` (compare bytes). With all inputs empty, the file opens with `row_count() == 0`.
- **Single input.** Tests that pass one input use `std::slice::from_ref(&a)`, not `&[a.clone()]`, which trips `clippy::cloned_ref_to_slice_refs`.
- **Errors.**
  - 0 inputs gives `Schema`.
  - Two inputs with different schemas give `Schema`.
  - A sort key naming a missing column gives `Schema`.
  - Inputs written with sort key `["start_time"]`, merged with `["trace_id", "start_time"]`, give `Schema`.
  - An input sorted by `trace_id` only: `take_record_batch(&wide_batch(5), &UInt32Array::from(vec![1u32, 0]))`, whose rows share trace id 0, written with sort key `["trace_id"]`. Merged with `["trace_id", "start_time"]`, it gives `Unsorted`.
- **Send.** `fn assert_send<T: Send>(_: &T) {}` called on the `merge(..)` future, so M5's DataFusion use stays possible.

**Memory test** (`tests/merge_memory.rs`, one `#[ignore]` test `merge_memory_bound`, run in release).

- **Test data.** `testgen` is private and `#[cfg(test)]`, so include its source file directly: put `#![allow(dead_code)]` at the top of `tests/merge_memory.rs`, then `#[path = "../src/testgen.rs"] mod testgen;`, and call `testgen::wide_batch(n)`. `testgen.rs` imports only arrow. This was compiled and passes clippy.
- **Allocator.** The test file declares a `#[global_allocator]` struct wrapping `std::alloc::System`, with two statics `CUR` and `PEAK: AtomicUsize`. `#![forbid(unsafe_code)]` applies to the library crate only, not to this test crate. It implements:
  - `alloc`: `cur = CUR.fetch_add(size) + size`, then `PEAK.fetch_max(cur)`;
  - `alloc_zeroed`: the same;
  - `dealloc`: `CUR.fetch_sub(size)`;
  - `realloc`: subtract the old size, then add the new one, then update `PEAK`.

  Use `Ordering::Relaxed`. Edition 2024 warns on unsafe calls inside `unsafe fn` bodies, so wrap every `System` call in an `unsafe { }` block with a `// SAFETY:` comment saying it forwards the caller's layout to `System`. Put `#[allow(unsafe_code)]` on the impl.
- **Inputs.** Build 4 inputs from `wide_batch(1_000_000)` as in the unit test, written to `InMemory` and opened, and drop the batches.
- **Bound.**
  - Compute `largest`: the maximum over the columns of the summed `get_array_memory_size()` of `read_column` on all 4 inputs. Drop those arrays.
  - Then set `base = CUR` and `PEAK = base`, run `merge`, and read `peak = PEAK − base`.
  - Assert `peak ≤ 2 × largest + 48 × rows + largest / 2`, where `rows = 1_000_000`. The terms are:
    - `48 × rows` covers the 16-byte sources and the row-format keys of step 3;
    - `largest / 2` covers the output buffer and codecs.
  - Print every term.
- **If the assertion fails,** print the per-column sizes, and stop and report. Do not raise the allowance.

### Done when

```bash
cargo +1.99.1 test
cargo +1.99.1 clippy --all-targets -- -D warnings
cargo +1.99.1 fmt --check
cargo +1.99.1 test --release -- --ignored roundtrip_payload roundtrip_light scan_datagen datafusion_equivalence fast_paths merge_memory --nocapture
```

**Commit:** `strata: add merge by k-way sort and per-column re-encoding (M6.1)`

---

## M6.2: payload frame copy

**Goal:** when merging a payload column, an input frame whose chunks are all new to the output is copied into the output byte for byte, with no decompression or new compression. Its chunks keep consecutive ids, as S4 requires. Everything derived from the frames is rebuilt as usual: refs, trigram filters, edges and the header.

**Edit:** `strata/src/{writer.rs,merge.rs,file.rs,payload/write.rs,payload/frames.rs,payload/mod.rs}`.

### Steps

1. **Stored-segment fetch** (`file.rs`).
   - Move the body of `fetch` into a private helper:

     ```rust
     async fn fetch_with<T>(&self, segments: &[(SegmentRef, Kind)], kind: ScanKind,
                            f: impl Fn(&[u8], &SegmentRef, Kind) -> Result<T>) -> Result<Vec<T>>;
     ```

     `f` gets the slice of the whole segment, header included. The helper keeps the coalescing, the slicing, the checks and the counter updates.
   - `fetch` passes `|slice, r, k| decode_segment(slice, r.offset, k)` and copies nothing.
   - Add:

     ```rust
     /// Like `fetch`, but also returns each segment's stored bytes as in the file (header
     /// included), next to its decoded body.
     pub(crate) async fn fetch_stored(&self, segments: &[(SegmentRef, Kind)], kind: ScanKind) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;
     ```

     It passes `|slice, r, k| Ok((slice.to_vec(), decode_segment(slice, r.offset, k)?))`, so every stored slice is also checked.

2. **Split the payload writer** (`payload/write.rs`). `write_payload_column` becomes:
   - **phase A**, which fills a `FrameBuilder` and the rows' id lists (`Vec<Option<Vec<u32>>>`) and counts `chunk_refs`;
   - **phase B**, `pub(crate) fn finish_payload_column(mut frames: FrameBuilder, rows: Vec<Option<Vec<u32>>>, chunk_refs: u64, codec: &mut SegmentCodec, frame_codec: &mut SegmentCodec, out: &mut Out) -> Result<(SegmentRef, PayloadStats)>`. It starts with `frames.finish(frame_codec, out)?` and holds everything after it: refs pages, pairs, filters, rollup, edges and the header.

   Behaviour is unchanged, and GOLDEN must not change.

3. **Frame copy in the builder** (`payload/frames.rs`).
   - **Trigram helper.** Move the trigram loop of `close` into `fn chunk_trigrams<'a>(chunks: impl Iterator<Item = &'a [u8]>) -> HashSet<u32>`, and have `close` use it.
   - **New methods:**

     ```rust
     /// Whether a chunk with this hash is already stored.
     pub(crate) fn has(&self, hash: u128) -> bool;
     /// Closes the open frame (if any), then appends `stored` (a whole PayloadFrame segment, header
     /// included, already checked) to `out` unchanged. Its chunks are `raw[ranges[k]]`; they get
     /// the next ids, which are returned in order.
     pub(crate) fn copy_frame(&mut self, stored: &[u8], raw: &[u8], ranges: &[Range<usize>],
                              frame_codec: &mut SegmentCodec, out: &mut Out) -> Result<Vec<u32>>;
     ```

   **`copy_frame`, in order:**
   1. Call `self.finish(frame_codec, out)?`, so the open frame is written and the ids stay consecutive. A copy right after `add` calls therefore closes a smaller frame early; that is harmless.
   2. Let `first = self.next_id` and `offset = out.len() as u64`. Append `stored` with `out.bytes(stored)`.
   3. For each range, in order:
      - take the chunk with `raw.get(range.clone())`, or `Corrupt { what: "copied frame", offset }`;
      - insert its `xxh3_128` hash with id `next_id`, only if the hash is not present (the caller made sure);
      - push its tail and head, as `add` does;
      - advance `next_id` with `checked_add`.
   4. Push `chunk_trigrams` of the chunks to `frame_trigrams`.
   5. Push `FrameEntry { offset, len: stored.len() as u32, raw_len: raw.len() as u32, first_chunk: first, n_chunks: ranges.len() as u32 }`, converting with `u32::try_from`; an overflow is `Unsupported`.
   6. Update the counters:
      - `stored_bytes += stored.len()`;
      - `raw_total += raw.len()`, the whole raw body, count and end offsets included;
      - `stored_total += stored.len().saturating_sub(SEGMENT_HEADER_LEN)`, with the constant from `crate::format`;
      - `ratio = raw_total / stored_total` when `stored_total > 0`.
   7. Set `cur_first = next_id`, and return `first .. next_id` as a `Vec`.

   **Why the copy stays valid.** A segment's checksum covers only its 12 header bytes and its stored bytes, not its offset (`segment.rs` `encode_segment`). So a copied segment stays valid at a new offset, and `FrameEntry.len == stored.len()`.

4. **Merging a payload column** (`merge.rs`, `payload/write.rs`, `writer.rs`).
   - **Types** (`payload/write.rs`; `payload/mod.rs` re-exports them as `pub(crate)`):

     ```rust
     /// One input frame kept for copying.
     pub(crate) struct CopiedFrame { pub stored: Vec<u8>, pub raw: Vec<u8>, pub ranges: Vec<Range<usize>>, pub first_chunk: u32 }
     /// The inputs of a merged payload column.
     pub(crate) struct PayloadCopy {
         /// Per input: its frames, in frame order.
         pub frames: Vec<Vec<CopiedFrame>>,
         /// Per input: each row's chunk ids (`None` for a null row).
         pub rows: Vec<Vec<Option<Vec<u32>>>>,
         /// The (input, row) source of each output row.
         pub sources: Arc<Vec<(usize, usize)>>,
     }
     /// A column for `write_file`.
     pub(crate) enum SourceColumn {
         /// A column to encode from its values.
         Array(ArrayRef),
         /// A merged payload column: its values, plus the inputs' frames for copying.
         Payload(ArrayRef, PayloadCopy),
     }
     ```

   - **`write_file`.** Its closure now returns `Result<SourceColumn>`, and `finish` wraps its arrays in `SourceColumn::Array`. `write_file` takes every decision from the array of either variant. When a column is payload (`is_payload(&array, config)`) **and** the source is `Payload`, it calls `write_payload_merged(copy, codec, frame_codec, out)` instead of `write_payload_column(array, ..)`.
   - **`write_payload_merged`** (`payload/write.rs`):
     1. **Frames.** Create a `FrameBuilder` and `remap: Vec<Vec<u32>>`, one empty `Vec` per input. For each input `i` in order, and each `CopiedFrame` in order:
        - check `first_chunk as usize == remap[i].len()`; otherwise `Corrupt { what: "merge chunk id", offset: 0 }`;
        - compute each chunk's hash once;
        - if no hash is in `has` and the frame's hashes are pairwise distinct, call `copy_frame` and extend `remap[i]` with the returned ids;
        - otherwise call `add(chunk)` for each chunk and push each id.
     2. **Rows.** For each `(input, row)` in `sources`, map the input row's ids with `remap[input].get(id as usize)`. A missing id is `Corrupt { what: "merge chunk id", offset: 0 }`. Count `chunk_refs`.
     3. **Finish.** Call `finish_payload_column`.

     Merge drops no row, so every input chunk stays referenced. The "referenced by at least one output row" condition of S10 therefore always holds, and is not checked.
   - **Building `PayloadCopy`** (`merge.rs`). For column `i`, the closure:
     1. builds the interleaved array as in M6.1, then drops `refs` and the input arrays;
     2. if `payload::is_payload(&array, &config)` is true and every kept input's directory record for `i` has `layer == Layer::Payload`, then for each kept input, concurrently:
        - `let header = read_header(file, i).await?` (an `Arc<PayloadHeader>`);
        - `fetch_stored(&frame_segments_of(&header, &(0..header.frames.len()).collect::<Vec<_>>())?, ScanKind::Scan)`. Check the exact `frame_segments_of` signature in `payload/read.rs`;
        - zip `header.frames` with the results and call `decode_fetched_frame(entry, raw)` on each, giving `raw` and `ranges`. `first_chunk` comes from the entry;
        - `read_rows(file, &header, record.payload_offset)`. Check its exact signature too.

        It returns `Payload(array, copy)`;
     3. otherwise returns `Array(array)`.

     The header, refs pages and frames are read twice: once by `read_column` and once here. This is an accepted cost of the prototype; say so in the doc comment.
   - **Re-exports.** `payload/mod.rs` re-exports `decode_fetched_frame`, `read_header`, `read_rows` and `frame_segments_of` as `pub(crate)` where not already done. It does **not** re-export `FrameBuilder`, which only `payload::write` uses: the import would be unused and fail clippy. For the boundary test, add `#[cfg(test)] pub(crate) use chunk::chunk_value;`.
   - **Memory.** The payload phase keeps the interleaved array plus every input's stored and raw frames. Add that sentence to § S10, which already says this phase is not bounded.

### Tests (`merge.rs`; `read_header`, `payload_contains` and `fetch_stored` are `pub(crate)`, so the tests live here)

- **Equivalence.** The M6.1 equivalence test, with `Config { payload_min_avg: 0, .. }`.
  - The inputs are **written** with that config, and `merge` is called with it too. Otherwise the inputs' columns are narrow and the copy path never runs.
  - Assert `directory()[col].layer == Layer::Payload` for `blob`, `uid` and `span_kind` in an input.
  - Compare **decoded columns only**: chunk ids, and therefore bytes, differ from a one-go write.
- **Frame copy.**
  - **Schema:** `[trace_id FixedSizeBinary(16) non-null, body Utf8 nullable]`, sort key `["trace_id"]`, and `merge(.., &["trace_id"], Config::default())`.
  - Input A holds even trace ids and input B odd ones, with ids `(k as u128).to_be_bytes()`. The output therefore alternates A0, B0, A1, B1 and so on.
  - Each input has 300 rows, whose `body` values are distinct random 2,000-byte lowercase strings from a seeded xorshift (non-zero seed). No value is shared. An average of 2,000 bytes is above 256, so `body` is payload.
  - Assert each input has at least 10 frames. A prototype run gave 21.
  - Merge.
  - **Assertions:**
    - the output has exactly `frames(A) + frames(B)` frames;
    - their stored bytes (`fetch_stored`) equal A's frames followed by B's, in order;
    - `decode_segment` of each stored slice equals the decoded body that `fetch_stored` returned.
  - Print the number of copied frames.
  - **Why this proves the copy.** A's frames would match even without the copy, because zstd is deterministic and the builder starts the same way. B's would not: the builder carries A's ratio and open frame into B. A prototype with the copy disabled matched 20 of 42 frames.
- **Shared chunks.** Two inputs hold the same 300 values, with different trace ids. Every frame of the second input is re-added, since all its chunks already exist. Assert:
  - the output's `summary.columns[body].payload` `unique_chunks` equals one input's;
  - the decoded column is correct.
- **Boundary trigrams after a copy.**
  - Two inputs of 40 rows each, with distinct random 30,000-byte strings, so each value has several chunks. Assert the output's `chunk_refs` is greater than its row count.
  - Merge.
  - `payload_contains(&file, col, needle).await?.rows` on the merged column equals brute force (`windows(16)` over each value) for:
    - every boundary needle `value[end − 8 .. end + 8]`, where `end` is the end of a non-final range of `chunk_value(value)`;
    - plus 10 random 16-byte substrings.
- **Memory.** The M6.1 memory test still passes. It uses the default config and `wide_batch`, which has no payload column, so it measures the narrow path only.

### Done when

The M6.1 commands pass. The commit body gives the number of copied frames from the frame-copy test.

**Commit:** `strata: copy payload frames byte for byte when merging (M6.2)`
