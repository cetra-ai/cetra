# Strata v0 · M1F: review fixes (robustness, compact encodings, speed)

Implementation plan for the fixes from the Sonnet code review of M1.1–M1.4 and M2.1. It runs after commit `e1d02f1` (the merge of M2.1) and before M2.2. There are three sub-phases (M1F.1–M1F.3), each done by one Haiku agent in one go, in order, each ending with the checks green and one commit.

**Read first:**
- [`01-m1-container-narrow.md`](01-m1-container-narrow.md): § Versions and § Conventions for every sub-phase. They all apply unchanged: build environment, checks, lints, no indexing, no panics, test layout.
- [`../prototype-plan.md`](../prototype-plan.md): § Conventions, S1, S3.
- The code in `docs/research/file-format/strata/src/`, as of `e1d02f1`.

**Why.** The review found no correctness bug. It found four problems:
1. **Forged-file allocation blowups.** A file with valid checksums but forged sizes can make the decoder allocate gigabytes.
2. **Size.** The light file is **1.87×** the size of default Parquet. About 70% of the gap comes from map columns stored as Arrow IPC, whose cumulative `i32` offsets do not compress. Most of the rest comes from the same cumulative-offset layout in `VARBIN` and `LIST_DICT`.
3. **Weak tests.** Several tests check less than the plan's test lists asked for.
4. **Benchmark-biasing slowness.** Hot loops do needless work.

The format is unreleased, so encodings change in place: no version bump, no compatibility path (AGENTS.md § No compatibility layers).

| Sub-phase | Content | Format change |
|---|---|---|
| M1F.1 | allocation caps, stricter validation, documented limits, test gaps | no (one writer behaviour change: no empty dictionary segment) |
| M1F.2 | lengths instead of offsets (`VARBIN`, `LIST_DICT`), a new `MAP` encoding, chunk budget by encoded width, zstd level 3 | yes |
| M1F.3 | speed: static gear table, reused zstd contexts, fewer per-row passes, one UTF-8 validation | no |

---

## M1F.1: robustness and tests

**Goal:**
- Every allocation and every output size in the decoders is bounded by constants, before allocating.
- Every offset and length read from the hot tail and the block tables is checked against the file layout.
- The tests check what the M1 plan's test lists asked for.

**Edit:** `strata/src/{format.rs,segment.rs,file.rs,reader.rs,writer.rs}`, `strata/src/narrow/{mod.rs,decode.rs,dict.rs,encode.rs}`, `strata/src/payload/chunk.rs` (tests only), `docs/research/file-format/prototype-plan.md` (§ Conventions, the limits paragraph).

### Steps

1. **Limits in `format.rs`.** These are format constants:

   ```rust
   /// Largest raw (decompressed) segment body the reader accepts, and the writer writes.
   pub const MAX_SEGMENT_RAW: u32 = 256 * 1024 * 1024;
   /// Largest number of rows in one narrow chunk.
   pub const MAX_CHUNK_ROWS: u32 = 65_535;                 // moved here from narrow/mod.rs
   /// Largest number of list elements in one narrow chunk.
   pub const MAX_CHUNK_ELEMENTS: usize = 1 << 20;          // was 1 << 24 in narrow/mod.rs
   /// Largest number of value bytes a decoded narrow chunk may materialise.
   pub const MAX_CHUNK_OUTPUT: usize = 256 * 1024 * 1024;
   ```

   - Delete the two old definitions in `narrow/mod.rs`, and import these from `crate::format`.
   - The writer never comes near these limits: chunks close at 4,096 rows and 32 KiB of value bytes.

2. **`segment.rs`.**
   - `encode_segment` returns `Unsupported` when `body.len() > MAX_SEGMENT_RAW as usize`. This replaces the `u32::MAX − 16` check.
   - `decode_segment` returns `Corrupt { what: "segment raw length" }` when `raw_len > MAX_SEGMENT_RAW`. This check comes before the checksum and before any decompression or allocation.
   - The check is also done when the codec is Raw, where `raw_len` must equal `stored_len`, otherwise `Corrupt { what: "segment raw length" }`.

3. **Decoder output budgets (`narrow/decode.rs`).**
   - **Dict and `LIST_DICT`:** in `decode_utf8` and `decode_list_utf8`, keep a running `out_bytes: usize` of the string bytes appended. Before each `append_value`, `out_bytes = out_bytes.checked_add(s.len())`. A failure, or a total above `MAX_CHUNK_OUTPUT`, is `Corrupt { what: "chunk output size" }`.
   - **`LIST_DICT` element count:** before `read_codes`, require `total ≤ MAX_CHUNK_ELEMENTS`. With the code width read first, also require `width == 0 || body bytes remaining ≥ ceil(total × width / 64) × 8`. `unpack` already checks the byte length exactly; this check runs before its `Vec` allocation. A width-0 list is still bounded by `MAX_CHUNK_ELEMENTS`, which is 1 M entries (8 MB of `u64`), acceptable.
   - **`FSB_RLE`:** before allocating the output, require `n.checked_mul(width) ≤ MAX_CHUNK_OUTPUT`, otherwise `Corrupt { what: "chunk output size" }`.
   - **`FSB_RAW`, `PLAIN`, `BOOL`, `VARBIN`:** these read exactly their input bytes, so their output is bounded by the segment size, and nothing changes.
   - **`Const`:** a `Const` of a variable-width type repeats `const_len ≤ 16` bytes `n ≤ 65,535` times, about 1 MB. That is fine.

4. **Layout validation (`file.rs`, `reader.rs`).**
   - Add `fn check_segment_range(&self, offset: u64, len: u32, what: &'static str) -> Result<()>` to `StrataFile`. It requires `offset ≥ HEADER_LEN`, `len ≥ SEGMENT_HEADER_LEN`, and `offset + len ≤ postscript.hot_offset` (checked arithmetic). Anything else is `Corrupt { what, offset }`.
   - Call it for:
     - every `PageRef` in `block_entries`;
     - every `BlockEntry` with `segment_len > 0`;
     - the dictionary in `read_column`;
     - inside `read_segment` itself, so every caller is covered.
   - `block_entries` also checks:
     - every page decodes to 1..=`Config::default().page_max_entries` entries;
     - the page's `PageRef.first_row` equals its first entry's `first_row`.
   - `StrataFile::open` checks that `directory[i].column_id == i` for every record, and that `layer` is `Narrow` or `Payload` for records `0 .. n_schema_fields`. M3 adds the other layers. Anything else is `Corrupt`.
   - `read_column` uses the dictionary only when flag bit 0 is set. The bit set with `dict_offset == 0`, or the bit clear with `dict_offset != 0`, is `Corrupt { what: "dictionary flag" }`.

5. **Writer.** `for_column` returns `None` when the dictionary would have zero values: an empty file, or an all-null column. No empty dictionary segment is written.

6. **Design doc.** In `prototype-plan.md` § Conventions (all structures), add a "Limits" bullet listing the four constants of step 1 and what each bounds. These limits are format rules: a file breaking them is corrupt.

### Tests

**Sliced arrays** (replaces `decode.rs` `sliced_arrays_round_trip`):
- A proptest (`with_cases(200)`) builds a random array of 0..300 rows with nulls for each of these types: Int64, Int16, UInt32, Float64, Float32, Timestamp(ns, "UTC"), Boolean, FixedSizeBinary(16), Utf8 with a dictionary, Utf8 without one (`VARBIN`), Binary, List(Utf8) with a dictionary, and Map(Utf8, Utf8) (`IPC`).
- It slices each array at `skip in 0..70` and `len in 0..=(rows − skip)`, runs `encode_chunk` then `decode_chunk`, and compares the result with the slice. Floats are compared bitwise with the existing helper.

**Float zones:**
- `[0.0, 1.5, NaN, null]`: the zone `min` is 0.0, `max` is 1.5, and `HAS_NAN` is set.
- `[-0.0, 2.0]`: the zone `min` bytes equal `0.0f64.to_le_bytes()` (normalised).
- A constant `-0.0` chunk: `Const`, `const_len = 8`, `min[0..8] == (-0.0f64).to_le_bytes()`, `HAS_ZONE` clear.
- A constant NaN chunk: `Const`, `HAS_NAN` set, `HAS_ZONE` clear.
- The same three cases for Float32 (`const_len = 4`).
- Each decodes back bitwise.

**Segment fuzzing.** Add a proptest (`with_cases(500)`) that builds a header with the right magic, a random kind and codec in their valid ranges, random `raw_len` and `stored_len` (including above `MAX_SEGMENT_RAW`), and random stored bytes. It computes a **correct** checksum, so the zstd and length branches are reached, and calls `decode_segment`. The result is `Ok` or `Err`, never a panic, and it never allocates more than `MAX_SEGMENT_RAW`. A `raw_len` above the cap gives `Corrupt { what: "segment raw length" }`.

**Block entries and pages:**
- A proptest over random 56-byte inputs to `BlockEntry::decode`, and random section-7 bytes to `pages_of`: never a panic.
- On a real file (`testgen::batch(20_000)`), patch one `PageRef.first_row` in section 7, recompute the hot-tail checksum, and write the file back. `block_entries` gives `Corrupt`.
- On the same file, patch a page's `offset` to point inside the hot tail. `block_entries` gives `Corrupt`.

**Decoder fuzz.** Extend `random_entries_and_bodies_never_panic`:
- `n_rows` drawn from `0..70_000`, which includes values above `MAX_CHUNK_ROWS`;
- the bodies built with a valid prefix (a correct validity bitmap and a valid width byte), followed by random bytes;
- the Utf8, Binary and List(Utf8) kinds included, with and without a dictionary.

**Allocation caps:**
- A `LIST_DICT` body with width 0 and `total = MAX_CHUNK_ELEMENTS + 1` gives `Corrupt`.
- A `Dict` chunk of 65,535 rows over a dictionary with one 64 KiB value gives `Corrupt { what: "chunk output size" }`: 65,535 × 64 KiB exceeds 256 MiB.

**Open on a populated file.** For `testgen::batch(20_000)` written with `page_max_entries = 1`, which makes a large hot tail:
- with `config.open_default_suffix = 4096` and no hint, open makes exactly 2 `get`s;
- with the exact hint, 1;
- every column round trips after either open.

**Writer:**
- Equal full sort keys across a batch boundary are accepted.
- A null in a sort-key column gives `Schema`.
- An all-null Utf8 column writes no dictionary segment: directory `dict_offset == 0`, flag bit 0 clear.

**`Const` with and without a dictionary.** One Utf8 column whose chunks are all `"abc"`:
- with a dictionary, it is `Const` with `const_len = 4` (a code);
- with `dict_max_values = 0` (no dictionary), it is `Const` with `const_len = 3` and the bytes `"abc"`.

**M2.1 coverage gap (`payload/chunk.rs`).** Add a third value strategy to the `chunk_value_invariants` proptest: JSON arrays of 1..6 string elements of 8,000..20,000 chars. This exercises the gear-inside-JSON branch with `base ≠ 0`. Invariants 1–4 hold for every case, and invariant 5 is skipped as specified.

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

**Goal:** remove cumulative offsets from every narrow encoding, and store maps natively. Lengths are stored relative to a base and bit-packed, which zstd compresses well. Target: the light file at most **1.35×** `pq_default`, down from 1.87×, which `roundtrip_light` asserts.

**Edit:**
- `strata/src/narrow/{mod.rs,encode.rs,decode.rs,chunker.rs,lengths.rs (new)}`;
- `strata/src/config.rs` (the zstd default);
- `strata/tests/roundtrip_light.rs`;
- `docs/research/file-format/prototype-plan.md` (S3 encodings table).

### The lengths block (`narrow/lengths.rs`)

A **lengths block** stores `n` non-negative lengths:

```
u32 base · u8 width · packed (len − base), n values, with pack() at `width`
```

- `base` is 0 when `has_nulls`, otherwise the minimum of `lens` (0 when `n = 0`).
- `width = width_for(max(len − base))`.

```rust
pub(crate) fn encode_lengths(lens: &[u32], has_nulls: bool, out: &mut Vec<u8>);
pub(crate) fn decode_lengths(c: &mut Cursor<'_>, n: usize, at: u64) -> Result<Vec<u32>>;
```

`decode_lengths`:
- reads `base` and `width` (`width > 32` is `Corrupt`, since lengths are `u32`);
- unpacks `n` values;
- adds `base` with `checked_add`, then `u32::try_from`; a failure is `Corrupt { what: "lengths" }`.

Nulls are not reapplied here. The caller checks that null slots have length 0 relative to `base` where the encoding requires it (see each encoding).

### Encodings (`Encoding` codes stay; one is added)

| Code | Name | New body (after the validity bitmap, if `HAS_NULLS`) |
|---|---|---|
| 7 | `VARBIN` | lengths block of the `n` value lengths · the value bytes, concatenated |
| 8 | `LIST_DICT` | lengths block of the `n` list lengths (null lists 0) · `u8 width` · packed codes of all elements |
| 11 | `MAP` | see below |

**Null slots.** One rule for `VARBIN`, `LIST_DICT` and every lengths block in `MAP`:
- When a chunk (or, for `MAP`, the value column) has any null slot, the writer uses `base = 0`, and stores length 0 for each null slot.
- Without nulls, `base` is the minimum length.
- The decoder rejects a null slot whose decoded length is not 0 as `Corrupt { what: "null length" }`.
- `encode_lengths` therefore takes the lengths with null slots already set to 0, plus a `has_nulls: bool`, and uses `base = 0` when it is true.

**`MAP` (code 11)** applies to `Map<Utf8, Utf8>` with non-nullable keys and nullable values: the schema of `attributes`, `resource_attributes` and `metadata`. Other map types stay `IPC`.

```
[validity bitmap of the maps, if HAS_NULLS]
lengths block: entry count per map (null maps 0)                       — n values
lengths block: key length per entry                                    — E values (E = total entries)
key bytes, concatenated
u8 value_nulls (0 = no null value, 1 = a value-validity bitmap of E bits follows)
[value-validity bitmap, ceil(E/8) bytes, Arrow layout, if value_nulls = 1]
lengths block: value length per entry (null values 0)                  — E values
value bytes, concatenated
```

- `E ≤ MAX_CHUNK_ELEMENTS`, otherwise `Unsupported` when writing and `Corrupt` when reading.
- The decoder:
  - rebuilds the offsets by prefix sum with `checked_add`;
  - checks every count against the remaining body before allocating;
  - checks that the key bytes and the value bytes are each valid UTF-8 (one check per buffer);
  - builds the `MapArray` with the field names, `keys_sorted` flag and nullability of the schema's map type, using `MapArray::try_new` (never a panicking constructor);
  - counts output bytes against `MAX_CHUNK_OUTPUT`.
- An all-null chunk is `CONST_NULL`, as for every type. There is no `Const` for maps.

### Steps

1. **`lengths.rs`:** the two functions above, plus unit tests.
2. **`encode.rs`:**
   - Rewrite `encode_varbin` and `encode_list_utf8` to emit the new bodies. The rules for `Const`, `CONST_NULL` and the `IPC` fallback (a null element inside a list) are unchanged.
   - Add `encode_map`, and dispatch `DataType::Map(entries, _)` to it when the entries struct is `(Utf8 non-null, Utf8 nullable)`; otherwise `IPC`.
3. **`decode.rs`:** the matching decoders.
   - `decode_varbin` builds the `i32` offsets by prefix sum over the decoded lengths, keeps every existing check, and validates UTF-8 once over the whole value buffer (`std::str::from_utf8(bytes)`), then per-value boundaries through `StringArray::try_new`.
   - `decode_list_utf8` rebuilds the list offsets the same way.
   - Add `decode_map`.
   - Add `Encoding::Map = 11` to the enum and to `from_u8`.
4. **`chunker.rs`: chunk budget by encoded width.**
   - For a Utf8 column **with a dictionary**, a row counts 4 bytes (one code), not `len + 4`.
   - For a List(Utf8) column with a dictionary, a row counts 4 bytes plus 4 per element.
   - `MAP` columns use the per-row size `4 + Σ (key len + value len + 8)` over the row's entries, instead of the IPC 256-row block measure.
   - `chunk_ranges` already receives `has_dict`; use it.
5. **`config.rs`:** change the `zstd_level` default from 1 to **3**, and update its doc.
6. **`roundtrip_light.rs`:** after printing the table, assert `strata_total ≤ pq_default_total × 135 / 100`. Print the per-column table as before.
7. **Design doc S3:** replace the `VARBIN` and `LIST_DICT` rows with the new bodies, add the `MAP` row, and add a "lengths block" paragraph. Remove the S3 sentence saying maps use `IPC`; the residual map of S6 still uses `IPC` until M3 revisits it.

### Tests

- **`lengths.rs`:** round trip of empty, all-equal (width 0), mixed, and `u32::MAX` lengths. Width 33 gives `Corrupt`. Never panic on random bytes.
- **Proptests per encoding** (`with_cases(200)`) round trip through `encode_chunk` and `decode_chunk`:
  - `VARBIN` (Utf8 and Binary, nulls, empty strings, a 64 KiB value);
  - `LIST_DICT` (null lists, empty lists, constant length 1);
  - `MAP` (null maps, empty maps, null values, empty keys, multi-byte UTF-8, 0..50 entries per row).
- **Size checks** (unit tests, asserting sizes so a regression is caught):
  - a 4,096-row `LIST_DICT` chunk where every list is `["openinference"]` has a body ≤ 64 bytes before zstd;
  - a 4,096-row `MAP` chunk where every row holds the same 6 entries has a zstd-compressed body ≤ 2 KiB.
- **Never panic:** random bodies passed to `decode_chunk` with `Encoding::Map` and the map field.
- A map with a null **key** cannot occur (the keys are non-nullable in the schema). A schema whose entries are `(Utf8 nullable, Utf8)` falls back to `IPC`.
- `roundtrip_light`: every column equal, and the ≤ 1.35× assertion.

### Done when

The M1F.1 commands pass, including `roundtrip_light` with its new assertion. The commit body holds the new size table next to the M1.4 table.

**Commit:** `strata: store lengths instead of offsets, add the MAP encoding (M1F.2)`

---

## M1F.3: speed

**Goal:** remove the hot-loop costs the review found. Nothing changes in the format, and the outputs are byte-identical (checked).

**Edit:** `strata/src/{segment.rs,writer.rs,reader.rs,file.rs}`, `strata/src/narrow/{encode.rs,decode.rs,chunker.rs,dict.rs}`, `strata/src/payload/chunk.rs`.

### Steps

1. **Gear table at compile time (`payload/chunk.rs`).**
   - Replace the `OnceLock` with `static GEAR_TABLE: [u64; 256] = gear_table();`, where `gear_table` is a `const fn` with the same xorshift loop. Use a `while` loop, since `for` is not allowed in `const fn`.
   - `gear_entry(b)` becomes `GEAR_TABLE.get(usize::from(b)).copied().unwrap_or(0)`.
   - The reference-equality proptest must still pass.
2. **Reused zstd contexts (`segment.rs`).**
   - Add `pub(crate) struct SegmentCodec { compressor: zstd::bulk::Compressor<'static>, level: i32 }` with `fn new(level) -> Result<Self>`.
   - `encode_segment` takes `&mut SegmentCodec` instead of `level: i32`. The writer creates one per distinct level: one for `config.zstd_level`. M2.2 adds one for frames.
   - For decoding, use a `thread_local! { static DECOMPRESSOR: RefCell<Option<zstd::bulk::Decompressor<'static>>> }`, created on first use; `decompress` goes through it. A `RefCell` borrow failure cannot happen (no re-entrancy), but handle it by falling back to `zstd::bulk::decompress`, with no `expect`.
   - A Raw segment's body is returned with `to_vec()` as before. A zero-copy path is M4 scope.
3. **Chunker without per-row vectors (`chunker.rs`).**
   - Fixed-width types compute their ranges arithmetically: `rows_per_chunk = clamp(max_bytes / width, 1, max_rows)`.
   - Variable-width types iterate the value offsets lazily (`value_offsets().windows(2)`) and accumulate, without building a `Vec<usize>`.
   - The ranges must be identical to before: keep the old function under `#[cfg(test)]` as `row_sizes_reference`, and add a proptest comparing them.
4. **Validity and values without per-row loops (`encode.rs`).**
   - `validity_bitmap` copies the array's null buffer bits: `array.nulls().map(|n| n.inner().sliced())`, which re-aligns to bit 0.
   - The values of primitive types come from `array.to_data().buffers()` with the data offset applied, as `read_bits` already does. Merge `bits`, `valid` and `deltas` into a single pass that computes min and max, then a second pass that packs.
   - Outputs must be byte-identical: add a proptest comparing `encode_chunk` bodies with the M1F.2 implementation, kept under `#[cfg(test)]` as `encode_chunk_reference`, for every type.
5. **UTF-8 validated once.**
   - `Dictionary` stores its values as `Vec<String>`, validated in `decode` and built from `&str` in `build`. `code_str` returns `&str` without re-validating.
   - In `decode_varbin`, validate the whole buffer once (M1F.2), then build with `StringArray::try_new`. Do not call `from_utf8` per row.
6. **Dictionary building stops early (`dict.rs`).** `build` keeps a running byte total (value bytes + 4 per value) while inserting into the `BTreeSet`, and returns `None` as soon as either limit is passed.

### Tests

- The reference-equality tests of steps 3 and 4.
- **Identical output.** Before changing any code, run the M1F.2 code once to compute the `xxh3_64` hash of `StrataWriter::finish` output for `testgen::batch(20_000)` with `Config::default()`. Record it in a new unit test as a constant, and assert that the M1F.3 code produces the same hash.
- **Timing.** A `#[ignore]` test `writer_throughput` writes `testgen::batch(200_000)` and prints MB/s of Arrow input. Run it in release before and after the changes, and report both numbers in the commit body. Nothing is asserted.

### Done when

The M1F.1 commands pass, and the commit body has the before and after `writer_throughput` numbers.

**Commit:** `strata: speed up chunking, compression contexts and validation (M1F.3)`
