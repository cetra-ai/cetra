# Strata v0 · M1F: review fixes (robustness, compact encodings, speed)

Implementation plan for the fixes from the Sonnet code review of M1.1–M1.4 and M2.1. It runs after commit `e1d02f1` (the merge of M2.1) and before M2.2. There are three sub-phases (M1F.1–M1F.3), each done by one Haiku agent in one go, in order, each ending with the checks green and one commit.

**Revision 2** applies the Sonnet cold review of revision 1: 8 blockers, 11 majors and the minors.

**Read first:**
- [`01-m1-container-narrow.md`](01-m1-container-narrow.md): § Versions and § Conventions for every sub-phase. They all apply unchanged: build environment, checks, lints, no indexing, no panics, test layout.
- [`../prototype-plan.md`](../prototype-plan.md): § Conventions, S1 and S3.
- The code in `docs/research/file-format/strata/src/`.

**Why.** The review found no correctness bug. It found four problems:
1. **Allocation blowups on forged files:** files with valid checksums but forged sizes can make the reader allocate gigabytes.
2. **Size:** the light file is 1.87× the size of default Parquet. About 70% of the gap comes from map columns stored as Arrow IPC, whose cumulative `i32` offsets do not compress. Most of the rest comes from the same cumulative offsets in `VARBIN` and `LIST_DICT`.
3. **Weak tests:** several tests check less than the M1 plan's test lists ask for.
4. **Slowness that would bias the benchmark.**

The format is unreleased, so encodings change in place, with no version bump and no compatibility path (AGENTS.md § No compatibility layers).

| Sub-phase | Content | Format change |
|---|---|---|
| M1F.1 | allocation caps, layout validation, documented limits, test gaps | no (two writer checks are added, and an all-null Utf8 column no longer gets an empty dictionary) |
| M1F.2 | lengths instead of offsets (`VARBIN`, `LIST_DICT`), a new `MAP` encoding, chunk budget by encoded width, zstd level 3 | yes |
| M1F.3 | speed: compile-time gear table, reused zstd contexts, no per-row vectors, one UTF-8 validation | no; the output is checked byte for byte |

---

## M1F.1: robustness and tests

**Goal:** every allocation and output size in the decoders is bounded by format constants **before** allocating; every offset and length read from the hot tail and the block tables is checked against the file layout; the tests check what the M1 plan's test lists asked for.

**Edit:**
- `strata/src/{format.rs,segment.rs,file.rs,reader.rs,writer.rs,config.rs}`;
- `strata/src/narrow/{mod.rs,decode.rs,dict.rs,encode.rs}`, plus every other file that imports `MAX_CHUNK_ROWS` or `MAX_CHUNK_ELEMENTS`;
- `strata/src/payload/chunk.rs` (tests only);
- `docs/research/file-format/prototype-plan.md` (§ Conventions).

### Steps

1. **Limits in `format.rs`.** These are format constants:

   ```rust
   /// Largest raw (decompressed) segment body the reader accepts and the writer writes.
   pub const MAX_SEGMENT_RAW: u32 = 256 * 1024 * 1024;
   /// Largest number of rows in one narrow chunk.
   pub const MAX_CHUNK_ROWS: u32 = 65_535;
   /// Largest number of list or map elements in one narrow chunk.
   pub const MAX_CHUNK_ELEMENTS: usize = 1 << 20;
   /// Largest number of value bytes a decoded narrow chunk may materialise.
   pub const MAX_CHUNK_OUTPUT: usize = 256 * 1024 * 1024;
   /// Largest number of block-table entries in one page.
   pub const MAX_PAGE_ENTRIES: usize = 4_096;
   /// Largest number of values in one dictionary.
   pub const MAX_DICT_VALUES: usize = 1 << 20;
   ```

   - Delete the definitions of `MAX_CHUNK_ROWS` and `MAX_CHUNK_ELEMENTS` (which was `1 << 24`) in `narrow/mod.rs`, and import these from `crate::format` wherever they are used (`writer.rs`, `narrow/encode.rs`, `narrow/decode.rs`, `narrow/chunker.rs`; find them with `grep`).
   - **`StrataWriter::new`** additionally returns `Schema` for `config.page_max_entries == 0 || > MAX_PAGE_ENTRIES`, and for `config.dict_max_values > MAX_DICT_VALUES`. Update both `Config` field docs to state the limit.

2. **`segment.rs`.**
   - `encode_segment` returns `Unsupported` when `body.len() > MAX_SEGMENT_RAW as usize`. This replaces the `u32::MAX − 16` check.
   - `decode_segment` checks, in this order, right after reading the header fields:
     1. `raw_len > MAX_SEGMENT_RAW` → `Corrupt { what: "segment raw length" }`;
     2. the existing stored-length check;
     3. for codec Raw, `raw_len != stored_len` → `Corrupt { what: "segment raw length" }`;
     4. the checksum;
     5. decompression.

3. **Decoder output budgets (`narrow/decode.rs`).**
   - **Dict-backed Utf8** (`decode_utf8`, and `decode_const` for Utf8 with a dictionary) and **`decode_list_utf8`**: two passes.
     1. Sum the byte length of the dictionary value of every element (every valid row for `decode_utf8`, `n` repetitions for `decode_const`), with `checked_add`. If the sum overflows or exceeds `MAX_CHUNK_OUTPUT`, return `Corrupt { what: "chunk output size" }`.
     2. Create `StringBuilder::with_capacity(item_count, total)` and append. Nothing is allocated before the check passes.
   - **`decode_list_utf8`:** after reading the offsets, require `total ≤ MAX_CHUNK_ELEMENTS` before `read_codes`; this check already exists, and only the constant changes.
   - **`FSB_RLE`:** before allocating the output, require `n.checked_mul(width) ≤ MAX_CHUNK_OUTPUT`, else `Corrupt { what: "chunk output size" }`.
   - **`Dictionary::decode`** (`dict.rs`): reject `n > MAX_DICT_VALUES` as `Corrupt { what: "dictionary count" }` before the offsets loop.
   - `FSB_RAW`, `PLAIN`, `BOOL`, `VARBIN` and `IPC` read exactly their input bytes, so they are bounded by `MAX_SEGMENT_RAW`. An inline `Const` repeats ≤ 16 bytes ≤ 65,535 times. Neither changes.

4. **The writer stays within the limits** (`narrow/encode.rs`). `encode_utf8` and `encode_list_utf8` compute the same decoded-bytes total as step 3, from the dictionary values of the chunk's codes. They return `Unsupported { what: "chunk output above MAX_CHUNK_OUTPUT" }` when it exceeds `MAX_CHUNK_OUTPUT`. In practice the 64 KiB dictionary cap and 4,096-row chunks keep this out of reach. The check makes the guarantee explicit.

5. **Layout validation.**
   - **`file.rs`, `open`:**
     - require `directory.len() ≥ schema.fields().len()`, else `Corrupt { what: "directory count" }`;
     - for each record `i`: `column_id == i`, else `Corrupt { what: "directory column id" }`;
     - for `i < schema.fields().len()`, `layer` is `Narrow` or `Payload`, else `Corrupt { what: "directory layer" }`. M3 adds the map layers.
   - **`file.rs`, `read_segment`:** before fetching, check `r.offset ≥ HEADER_LEN`, `u64::from(r.len) ≥ SEGMENT_HEADER_LEN as u64`, and `r.offset + u64::from(r.len) ≤ postscript.hot_offset`, with checked arithmetic. Otherwise return `Corrupt { what: "segment range", offset: r.offset }`. This covers every segment read: pages, chunks, dictionaries, and later the payload segments.
   - **`file.rs`, `block_entries`:** check each `PageRef` and each entry with `segment_len > 0` with the same range rule, when the table is loaded, so a lookup can trust it. Every page decodes to `1..=MAX_PAGE_ENTRIES` entries, and its `PageRef.first_row` equals the `first_row` of its first entry. Otherwise return `Corrupt { what: "block page" }`.
   - **`reader.rs`:** use the dictionary only when directory flag bit 0 is set. The bit set with `dict_offset == 0`, or clear with `dict_offset != 0`, is `Corrupt { what: "dictionary flag" }`.

6. **Writer: no empty dictionary for Utf8.** In `dict.rs` `for_column`, a **Utf8** column whose dictionary would have zero values (no non-null value) gets `None`, so no dictionary segment is written.

   **List(Utf8) is unchanged:** a List(Utf8) column with no element keeps its empty dictionary, and so stays `LIST_DICT`. Without one, `uses_ipc` would send it to `IPC`.

7. **Design doc.** In `prototype-plan.md` § Conventions (all structures), add a **Limits** bullet listing the six constants of step 1 and what each bounds. These are format rules: a file breaking them is corrupt.

### Tests

**Sliced arrays** (replaces `decode.rs` `sliced_arrays_round_trip`).
- A proptest (`with_cases(100)`) builds, for each of these types, a random array of 0..300 rows with nulls:
  - numerics: Int64, Int16, UInt32, Float64, Float32;
  - Timestamp(ns, "UTC"), Boolean, FixedSizeBinary(16);
  - strings: Utf8 with a dictionary, Utf8 without one, Binary;
  - lists and maps: List(Utf8) with a dictionary, and Map(Utf8 non-null, Utf8 nullable).
- It slices the array at `skip in 0..70` and `len in 0..=(rows − skip)`, builds the dictionary from the **full** array, runs `encode_chunk` then `decode_chunk`, and compares the result with the slice. Floats are compared bitwise with the existing helper.

**Float zones** (`encode.rs`). Each case also decodes back bitwise:
- `[0.0, 1.5, NaN, null]`: zone `min` bytes = `0.0f64.to_le_bytes()`, `max` = `1.5f64.to_le_bytes()`, `HAS_NAN` set.
- `[-0.0, 2.0]`: `min` bytes = `0.0f64.to_le_bytes()`.
- A constant `-0.0` chunk: `Const`, `const_len = 8`, `min[0..8] == (-0.0f64).to_le_bytes()`, `HAS_ZONE` clear.
- A constant NaN chunk: `Const`, `HAS_NAN` set, `HAS_ZONE` clear.
- The same constant cases for Float32, with `const_len = 4`.

**Segment fuzz** (`segment.rs`, `with_cases(500)`).
- Build a header with the right magic, a random valid kind and codec, a random `raw_len` (sometimes above `MAX_SEGMENT_RAW`), `stored_len` = the number of random stored bytes (0..2,048), and a **correct** checksum over header bytes 0..12 and the stored bytes. Then call `decode_segment`.
- The result is `Ok` or `Err`, never a panic. A `raw_len` above the cap gives `Corrupt { what: "segment raw length" }`.

**Block entries and pages.**
- A proptest gives random 56-byte inputs to `BlockEntry::decode`, and random section-7 bytes to `HotTail::pages_of` (with a hand-built directory record): never a panic.
- On a real file written from `testgen::batch(20_000)`, patch the hot tail and write the file back. Section 7 is the last hot section, so its bytes end at `file.len() − 64`. A `PageRef` is `first_row u32 · offset u64 · len u32`, and the hot checksum is postscript bytes 12..16 (= `checksum32(&[hot])`). After each patch, recompute the checksum.
  - Patching one `PageRef.first_row` makes `block_entries` return `Corrupt`.
  - Patching a page's `offset` to point inside the hot tail makes it return `Corrupt`.

**Decoder fuzz.** Extend `random_entries_and_bodies_never_panic`:
- draw `n_rows` from `0..70_000`, with `with_cases(200)` so debug time stays bounded;
- half the bodies are random bytes;
- the other half are a valid validity bitmap, a valid width byte, and then exactly the packed byte count for `n` codes, with random contents;
- include Utf8, Binary and List(Utf8), each with and without a dictionary.

**Allocation caps:**
- A `LIST_DICT` body with width 0 and offsets ending at `MAX_CHUNK_ELEMENTS + 1` gives `Corrupt`.
- A `Dict` chunk of 65,535 valid rows, all code 0, over a dictionary with one 64 KiB value, gives `Corrupt { what: "chunk output size" }` (65,535 × 64 KiB > 256 MiB).
- The same dictionary with a `Const` entry and `n_rows = 65,535` gives `Corrupt { what: "chunk output size" }`.
- `Dictionary::decode` with `n = MAX_DICT_VALUES + 1` gives `Corrupt { what: "dictionary count" }`.

**Open on a populated file** (`file.rs` tests, `testgen::schema()`, `testgen::batch(20_000)` written with `page_max_entries = 1`):
- Assert `u64::from(summary.hot_len) + 64 > 1024`.
- With `config.open_default_suffix = 1024` and no hint, open makes exactly 2 `get`s.
- With `Some(hot_len)`, it makes 1.
- Count them as `short_hint_makes_two_gets` does: `Counting::gets()` before and after.
- Every column round trips after either open.

**Writer:**
- `page_max_entries = 0` and `= MAX_PAGE_ENTRIES + 1`, and `dict_max_values = MAX_DICT_VALUES + 1`, each give `Schema`.
- Equal full sort keys across a batch boundary are accepted.
- A null in a sort-key column gives `Schema`. The test uses a schema whose `trace_id` field is **nullable**, since `RecordBatch::try_new` rejects nulls in a non-nullable field.
- An all-null Utf8 column writes no dictionary: its directory `dict_offset == 0`, and flag bit 0 is clear.

**`Const` with and without a dictionary.** One Utf8 column whose chunks are all `"abc"`:
- with a dictionary, it is `Const` with `const_len = 4` (a code);
- with `dict_max_values = 0` (no dictionary), it is `Const` with `const_len = 3` and the bytes `"abc"`.

**M2.1 coverage** (`payload/chunk.rs`). Add a third value strategy to `chunk_value_invariants`: JSON arrays of 1..6 string elements, each `"x".repeat(k)` for `k` in 8,000..20,000. The repeat-based strings keep debug time low. This exercises the gear-inside-JSON branch with `base ≠ 0`. Invariants 1–4 hold, and 5 is skipped as specified.

### Done when

```bash
cargo +1.99.1 test
cargo +1.99.1 clippy --all-targets -- -D warnings
cargo +1.99.1 fmt --check
cargo +1.99.1 test --release -- --ignored roundtrip_light
```

**Commit:** `strata: cap decoder allocations, validate layout offsets, close test gaps (M1F.1)`

---

## M1F.2: compact encodings

**Goal:** remove cumulative offsets from the narrow encodings, and store `Map<Utf8, Utf8>` natively. Lengths are stored relative to a base and bit-packed, which zstd compresses well. Target: the light file at most **1.35×** `pq_default` (it was 1.87×). If the target is missed, the sub-phase reports instead of tuning (step 9).

**Edit:**
- `strata/src/narrow/{mod.rs,encode.rs,decode.rs,chunker.rs,lengths.rs (new)}`;
- `strata/src/{config.rs,writer.rs,testgen.rs}`;
- `strata/tests/roundtrip_light.rs`;
- `docs/research/file-format/prototype-plan.md` (S3).

### The lengths block (`narrow/lengths.rs`)

A **lengths block** stores `n` lengths:

```
u32 base · u8 width · packed (len − base), n values, with pack() at `width`
```

```rust
/// `lens` has null slots already set to 0.
pub(crate) fn encode_lengths(lens: &[u32], out: &mut Vec<u8>);
pub(crate) fn decode_lengths(c: &mut Cursor<'_>, n: usize, at: u64) -> Result<Vec<u32>>;
```

**`encode_lengths`:**
- `base = min(lens)`, or 0 when `n = 0`. Because null slots are 0, `base` is 0 whenever there is a null.
- `width = width_for(max(len − base))`.

**`decode_lengths`:**
1. Read `base` (u32) and `width` (u8). `width > 32` is `Corrupt { what: "lengths" }`.
2. `packed = n.checked_mul(width).map(|b| b.div_ceil(64)).and_then(|w| w.checked_mul(8))`. An overflow is `Corrupt`.
3. Take exactly `packed` bytes with `c.bytes(packed, "lengths")`. A short body is `Corrupt`, raised before any allocation.
4. Call `unpack(bytes, n, width)`; its error becomes `Corrupt { what: "lengths", offset: at }`.
5. Add `base` to each value with `checked_add`, then `u32::try_from`. A failure is `Corrupt { what: "lengths" }`.

**Null rule.** In `VARBIN`, `LIST_DICT` and `MAP`, a null slot has length 0. The decoder rejects a null slot whose decoded length is not 0 as `Corrupt { what: "null length" }`.

### Encodings

| Code | Name | New body (after the validity bitmap, if `HAS_NULLS`) |
|---|---|---|
| 7 | `VARBIN` | lengths block of the `n` value lengths · the value bytes, concatenated |
| 8 | `LIST_DICT` | lengths block of the `n` list lengths · `u8 width` · packed codes of all elements |
| 11 | `MAP` | see below |

**The `Map<Utf8, Utf8>` shape.** Add to `narrow/mod.rs`:

```rust
/// True for `Map` columns that use the `MAP` encoding.
pub(crate) fn is_utf8_map(dt: &DataType) -> bool
```

It is true only when all of these hold:
- `dt` is `DataType::Map(f, _)`, and `!f.is_nullable()`;
- `f.data_type()` is `DataType::Struct(fields)` with exactly 2 fields;
- field 0 is `Utf8` and not nullable;
- field 1 is `Utf8` and nullable.

Encode, decode and the chunker all use this one predicate. Other map types stay `IPC`.

**`MAP` (code 11) body:**

```
[validity bitmap of the maps, if HAS_NULLS]
lengths block: entry count per map (null maps 0)                        — n values
lengths block: key length per entry                                     — E values (E = sum of counts)
key bytes, concatenated
u8 value_nulls (0: no null value; 1: a value-validity bitmap of E bits follows)
[value-validity bitmap, ceil(E/8) bytes, Arrow layout, if value_nulls = 1]
lengths block: value length per entry (null values 0)                   — E values
value bytes, concatenated
```

**Encoder (`encode_map`).**
- Downcast with `as_map_opt()`.
- Read each map's entries through `map.value_offsets()`, which are absolute indices into the unsliced `map.entries()`.
- Skip the entries of null maps; `E` counts entries of non-null maps only.
- A null value gets length 0, and its bit is clear in the value-validity bitmap.
- `E > MAX_CHUNK_ELEMENTS` returns `Unsupported`.
- An all-null chunk is `CONST_NULL`, as for every type. There is no `Const` for maps.

**Decoder (`decode_map`).**
1. Decode the counts, then `E = Σ counts` with `checked_add`. Require `E ≤ MAX_CHUNK_ELEMENTS` **before** decoding the next lengths block.
2. Check every byte count against the remaining body before allocating. The sum of key bytes plus value bytes must be ≤ `MAX_CHUNK_OUTPUT`.
3. Build the offsets by prefix sum with `checked_add`, then `i32::try_from`.
4. `let DataType::Map(entries_field, sorted) = dt`, then `let DataType::Struct(fields) = entries_field.data_type()`.
5. Build the keys and values as `StringArray::try_new(offsets, buffer, nulls)`. That call validates UTF-8; map its error to `Corrupt { what: "map utf8" }`.
6. `let entries = StructArray::try_new(fields.clone(), vec![Arc::new(keys), Arc::new(values)], None)`.
7. `MapArray::try_new(Arc::clone(entries_field), offsets, entries, nulls, *sorted)`. Map every error to `Corrupt`.

The field and fields come from the column's `Field`, so the arrays compare equal to the input.

### Steps

1. **`lengths.rs`:** the two functions above.

2. **`encode.rs`:**
   - Rewrite `encode_varbin` and `encode_list_utf8` to emit the new bodies. The rules for `Const`, `CONST_NULL` and the `IPC` fallback (a null element inside a list) are unchanged.
   - Add `encode_map`.
   - In `encode_chunk`, add a `DataType::Map(..) if is_utf8_map(dt)` arm before the final fallback.

3. **`decode.rs`:**
   - **`decode_varbin`:** decode the lengths, check the null rule, take `sum` bytes, build the `i32` offsets by prefix sum (every existing check is kept), and build with `StringArray::try_new` or `BinaryArray::try_new`. Do **not** call `from_utf8` separately: `try_new` validates UTF-8 and char boundaries. Its error becomes `Corrupt { what: "varbin utf8" }` for Utf8.
   - **`decode_list_utf8`:** rebuild the list offsets the same way.
   - **`decode_map`:** add it, with a `DataType::Map(..)` arm that requires `entry.encoding == Encoding::Map && is_utf8_map(dt)`, else `Corrupt { what: "encoding for type" }`. The `Encoding::Ipc` early return stays first, so IPC maps still decode.
   - Add `Encoding::Map = 11` to the enum and to `from_u8`.

4. **`writer.rs`:** `ColumnSummary.encoding_counts` and the local `counts` become `[u32; 12]`. Update the field's doc.

5. **`chunker.rs`: budget by encoded width.**
   - `uses_ipc` returns `false` for `is_utf8_map(dt)`.
   - Per-row byte budgets:

     | Column | Bytes per row |
     |---|---|
     | Utf8 with a dictionary | 4 |
     | List(Utf8) with a dictionary | 4 + 4 × elements |
     | `MAP` | 4 + Σ (key len + value len + 8) over the row's entries |
     | Utf8 and Binary without a dictionary | `len + 4` (unchanged) |

6. **`config.rs`:** change the `zstd_level` default from 1 to **3**, and update its doc to "Default 3".

7. **`testgen.rs`.** Add `pub(crate) fn wide_batch(n: usize) -> RecordBatch` (and `wide_schema()`), with every narrow type the format handles, for M1F.3's golden hash. Row *i* (all sorted by `trace_id` by construction):

   | Column | Value for row *i* |
   |---|---|
   | `trace_id` FixedSizeBinary(16) | as `batch` |
   | `start_time` | as `batch` |
   | `f32` Float32 | null when `i % 9 == 0`, else `i as f32 * 0.5` |
   | `short_id` FixedSizeBinary(8) | `(i as u64).to_le_bytes()` |
   | `blob` Binary | null when `i % 5 == 0`, else `i % 300` bytes of `(i % 251) as u8` |
   | `uid` Utf8 (high cardinality, so `VARBIN`) | `format!("user-{i:08}")` |
   | `attrs` Map(Utf8, Utf8 nullable) | null when `i % 13 == 0`; else `i % 4` entries `("k{j}", "v{i}-{j}")`, with the value null when `(i + j) % 7 == 0` |
   | the `batch` columns | every column of `batch(n)` |

   The map field is built as `Field::new("attrs", DataType::Map(Arc::new(Field::new("entries", DataType::Struct(vec![Field::new("keys", Utf8, false), Field::new("values", Utf8, true)].into()), false)), false), true)`.

8. **Existing tests made invalid by the format change.** Update or replace each:
   - `decode.rs` `varbin_with_decreasing_offsets_is_corrupt`: replace it with "a `VARBIN` lengths block whose sum exceeds the bytes is `Corrupt`".
   - `decode.rs` `maps_round_trip_as_ipc`: becomes `maps_round_trip_as_map`.
   - `decode.rs` `random_varbin_and_ipc_bodies_never_panic`: add `MAP`.
   - `encode.rs` `map_column_is_ipc`: assert `Map`, and add an `IPC` case for a map whose value entries are `(Utf8 nullable, Utf8)`.
   - `chunker.rs` `list_of_utf8_rows_count_elements_and_row`: under the new budget a row with 2 elements is 4 + 4 × 2 = 12 bytes, so with budget 32 the expectation becomes `[3, 1]` (recompute it from the test's data and state it in the test).
   - Every hand-built `VARBIN` or `LIST_DICT` body in tests, including the M1F.1 cap test: rebuild it with a lengths block.
   - The M1F.1 sliced-array test: its map case now exercises `MAP`.

9. **`roundtrip_light.rs`.**
   - After printing the table, assert `summary.bytes * 100 ≤ pq_total * 135`. These are the existing variable names; read the file for the exact ones.
   - Print `Map` in the encodings header.
   - **If the assertion fails, do not raise the threshold, and do not add encodings.** Commit nothing. Report the per-column table and the achieved ratio, then stop.

10. **Design doc S3:**
    - replace the `VARBIN` and `LIST_DICT` rows with the new bodies;
    - add the `MAP` row and the `is_utf8_map` shape;
    - add a "lengths block" paragraph with the null rule;
    - the zstd default is now 3.

### Tests

**`lengths.rs`:**
- Round trip of an empty list, all-equal lengths (width 0), mixed lengths, and `[0, u32::MAX]`.
- Width 33 gives `Corrupt`. A short body gives `Corrupt`.
- Never panic on random bytes.

**Round trips** (proptests, `with_cases(100)`, through `encode_chunk` and `decode_chunk`):
- `VARBIN`: Utf8 and Binary, with nulls, empty strings, and one 64 KiB value.
- `LIST_DICT`: null lists, empty lists, and constant length 1.
- `MAP`: null maps, empty maps, null values, empty keys, multi-byte UTF-8, and 0..50 entries per row.

**Sizes**, asserted so that a regression is caught:
- A 4,096-row `LIST_DICT` chunk where every list is `["openinference"]` has a body ≤ 64 bytes before zstd. Expected: a lengths block of 5 bytes, a width byte, and no codes.
- A 4,096-row `MAP` chunk where every row holds the same 6 entries — `("telemetry.sdk.name", "opentelemetry")`, `("telemetry.sdk.language", "python")`, `("telemetry.sdk.version", "1.27.0")`, `("host.name", "chat-api-3")`, `("process.pid", "1003")`, `("deployment.environment", "production")` — has `zstd::bulk::compress(&body, 3)` output ≤ 2 KiB.

**Encoding counts.** A map column's `ColumnSummary.encoding_counts[Encoding::Map as usize]` equals its chunk count.

**Chunker budgets.** Assert the ranges for each row of the step 5 table:
- Utf8 with a dictionary: 4 bytes per row, so 4,096 rows at budget 32 KiB;
- List(Utf8) with a dictionary: 4 + 4 per element;
- `MAP`: 4 + Σ (k + v + 8).

**Never panic:** random bodies for `decode_chunk` with `Encoding::Map` and the map field.

**`roundtrip_light`:** every column equal, and the 1.35× assertion.

### Done when

The M1F.1 commands pass. The commit body holds the new size table next to the M1.4 one.

**Commit:** `strata: store lengths instead of offsets, add the MAP encoding (M1F.2)`

---

## M1F.3: speed

**Goal:** remove the hot-loop costs the review found. Nothing in the format changes, and the writer's output is checked byte for byte against M1F.2.

**Edit:**
- `strata/src/{segment.rs,writer.rs,file.rs,error.rs}`;
- `strata/src/narrow/{encode.rs,decode.rs,chunker.rs,dict.rs}`;
- `strata/src/payload/chunk.rs`;
- `strata/tests/roundtrip_light.rs`, if it calls `encode_segment`;
- `docs/research/file-format/plans/02-m2-payload.md` (step 7).

### Steps

1. **Golden hash, before changing any code.**
   - Add a unit test `golden_output_hash` in `writer.rs`. It writes `testgen::wide_batch(20_000)` with `Config::default()` and computes `xxhash_rust::xxh3::xxh3_64(&finish().0)`.
   - Run it once on the M1F.2 code, and write the printed value into the test as `const GOLDEN: u64 = 0x…;`, asserted equal.
   - Commit nothing yet. Every later step must keep this test green.

2. **Gear table at compile time** (`payload/chunk.rs`). Replace the `OnceLock` with:

   ```rust
   #[allow(clippy::indexing_slicing)] // i < 256 by the loop bound
   const fn gear_table() -> [u64; 256] {
       let mut t = [0u64; 256];
       let mut x: u64 = GEAR_SEED;
       let mut i = 0;
       while i < 256 {
           x ^= x << 13;
           x ^= x >> 7;
           x ^= x << 17;
           t[i] = x;
           i += 1;
       }
       t
   }
   static GEAR_TABLE: [u64; 256] = gear_table();
   ```

   `gear_entry(b)` becomes `GEAR_TABLE.get(usize::from(b)).copied().unwrap_or(0)`. This is the one `allow` outside tests that this plan adds.

3. **Reused zstd contexts** (`segment.rs`).
   - Add:

     ```rust
     pub(crate) struct SegmentCodec { compressor: zstd::bulk::Compressor<'static> }
     impl SegmentCodec { pub(crate) fn new(level: i32) -> Result<Self> }
     ```

     A zstd initialisation error maps to `StrataError::Io { action: "zstd init", source }`.
   - `encode_segment(kind, body, prefer, codec: &mut SegmentCodec, out)` replaces the `level: i32` parameter, and compresses with `codec.compressor.compress(body)`.
   - The writer creates one `SegmentCodec::new(config.zstd_level)` in `finish` and passes `&mut` to every call. Update every call site, the tests included (`SegmentCodec::new(1)` there).
   - For decoding: `thread_local! { static DECOMPRESSOR: RefCell<Option<zstd::bulk::Decompressor<'static>>> = const { RefCell::new(None) }; }`. `decode_segment` creates the decompressor on first use and calls `.decompress(stored, raw_len as usize)`. If `try_borrow_mut` fails, fall back to `zstd::bulk::decompress`; there is no `expect`.
   - Compression output is identical to `zstd::bulk::compress` at the same level, so the golden hash holds.

4. **Chunker without per-row vectors** (`chunker.rs`).
   - **Fixed-width types:**

     ```rust
     let per = if width == 0 { max_rows } else { max_bytes.div_ceil(width) }.min(max_rows).max(1);
     ```

     Ranges are consecutive blocks of `per` rows. Never use `clamp`, which panics when `max_rows == 0`.
   - **Variable-width types** walk the rows lazily: `value_offsets().windows(2)` for Utf8, Binary and List, and the map offsets for `MAP`. They accumulate the same per-row sizes as M1F.2, with no `Vec<usize>`.
   - Keep the M1F.2 `row_sizes` function under `#[cfg(test)]` as `row_sizes_reference`. Add a proptest over `(n in 0..2000, type, max_rows in 0..5000, max_bytes in 1..70_000)`, `max_rows = 0` and `max_bytes = 1` included, comparing the new ranges with ranges computed from `row_sizes_reference` by the M1F.2 loop.

5. **Validity and values without per-row loops** (`encode.rs`).
   - `validity_bitmap` takes `nulls.inner().sliced()`, copies exactly `len.div_ceil(8)` bytes into a `Vec<u8>`, and clears the bits at and above `len % 8` in the last byte when `len % 8 != 0`. Arrow does not mask them on byte-aligned slices. It is called only when `null_count() > 0`.
   - The values of primitive types come straight from the buffers, as `read_bits` does. Merge the `bits`, `valid` and `deltas` passes into one pass that computes `min` and `max`, then one pass that packs.
   - **Null slots must still encode as 0**: delta 0 for `FOR_BITPACK`, zero bytes for `PLAIN` and FSB, bit 0 for `BOOL`. The buffer may hold anything at a null slot.
   - The golden hash (step 1) and the M1F.1 sliced-array proptest guard these changes.

6. **UTF-8 validated once, dictionary strings.**
   - `Dictionary` stores `Vec<String>`, validated once in `decode` and built from `&str` in `build`.
   - Keep `value(&self, code) -> Option<&[u8]>` (its tests use it), and add `str_value(&self, code: u32) -> Option<&str>`.
   - `code_str` uses `str_value` with no re-validation.

7. **Dictionary building stops early** (`dict.rs` `build`).
   - Keep a running byte total of the values **as they are inserted**: count a value only when `BTreeSet::insert` returns true.
   - Return `None` as soon as the number of values exceeds `max_values`, or `bytes + 4 × (count + 1)` exceeds `max_bytes`.
   - The final result must be unchanged; `build_limits_bytes_including_four_per_offset` still passes.

8. **M2.2 plan.** In `docs/research/file-format/plans/02-m2-payload.md`, M2.2:
   - § Config: "Default 1" becomes "Default 3".
   - Every `encode_segment(.., level, out)` call becomes `encode_segment(.., &mut codec, out)`.
   - The writer creates a second `SegmentCodec::new(FRAME_ZSTD_LEVEL)` for frames, passed to `write_payload_column` and `FrameBuilder`. Their signatures gain `frame_codec: &mut SegmentCodec` and `codec: &mut SegmentCodec` parameters. Write these signature changes into the M2.2 text.

### Tests

- `golden_output_hash` (step 1) passes after every step.
- The chunker reference proptest (step 4).
- **Throughput.** A `#[ignore]` test `writer_throughput` writes `testgen::wide_batch(200_000)` in release and prints MB/s, where MB is `batch.get_array_memory_size()` and the time covers `write` plus `finish`. Run it on the M1F.2 code (step 1) and after all steps, and report both numbers in the commit body. Nothing is asserted.

### Done when

The M1F.1 commands pass, and the commit body holds the before and after `writer_throughput` numbers.

**Commit:** `strata: speed up chunking, compression contexts and validation (M1F.3)`
