# Strata v0 · M3: key lookup, key filters and shredded maps

Implementation plan for phase 4 of [the prototype design](../prototype-plan.md), § S5 and S6. Three sub-phases, each done by one Haiku agent in one go, in order, each ending with the checks green and one commit.

**Revision 3.** Revision 2 applied the first Sonnet cold review (7 blockers, 12 majors and the minors). Revision 3 applies the second (3 blockers, 9 majors and minors). Per the review convention, there is no third round.

**Prerequisites:** M2.3 is committed.

**Read first:**
- [`01-m1-container-narrow.md`](01-m1-container-narrow.md): § Versions and § Conventions for every sub-phase. They apply unchanged: build environment (`CARGO_TARGET_DIR=$S/target-strata`), checks, lints, no indexing, no panics, test layout.
- [`02-m2-payload.md`](02-m2-payload.md): M2.2 § Rules for every step.
- [`../prototype-plan.md`](../prototype-plan.md): § S1 (hot-tail sections 4, 5 and 6, postscript), § S2, § S5 and § S6.
- The code in `docs/research/file-format/strata/src/`: `hot.rs`, `file.rs`, `writer.rs`, `reader.rs`, `narrow/`, `format.rs`, `testgen.rs`, `lib.rs`.

**Global rules:**
- `Corrupt` carries an `offset`: the base offset of the section or segment being decoded, or 0 when there is none (pure functions such as `shred`).
- `Unsupported.what` is a `String` (`"…".to_string()`).
- The current `GOLDEN` in `writer.rs` is whatever is committed.

| Sub-phase | Content |
|---|---|
| M3.1 | key filters (binary fuse) and sort-key lookup |
| M3.2a | `shred` / `unshred` as pure functions, with their tests; no file change |
| M3.2b | shredded maps in the file: layout, writer, open checks and reader |

The map-key **predicate** (`MapKeyEq`) and `scan` are M4. M3 only provides storage, `lookup`, `may_contain` and `read_column`.

---

## M3.1: key filters and sort-key lookup

**Goal:**
- **Key filters.** A file can answer "may this value occur in column C?" without reading data, for the first sort-key column and for configured columns (by default `user_id` and `session_id`).
- **Lookup.** It can find the row ranges of one sort key by reading only the sort-key chunks whose zones may hold it.

**Edit:**
- `strata/Cargo.toml` (one dependency);
- `strata/src/{config.rs,hot.rs,file.rs,writer.rs,lib.rs}`;
- `strata/src/index.rs` (new);
- `strata/tests/roundtrip_light.rs`;
- `docs/research/file-format/prototype-plan.md` (S1 postscript, S5).

**Dependency.** `xorf = { version = "0.13.0", default-features = false, features = ["binary-fuse"] }` (MIT, `cargo info xorf`). Imports: `use xorf::{BinaryFuse8, Descriptor, Filter};`. The `prelude` module is private.

### The key-filter body (`index.rs`)

```
u8 tag
tag 0  empty:   the column has no non-null value                       may_contain → Some(false)
tag 1  one:     u32 len · value bytes (len ≤ 1024)                     may_contain → Some(value == that)
tag 2  fuse:    u64 seed · u32 segment_length · u32 segment_length_mask · u32 segment_count_length
                · u32 n_fingerprints · fingerprints[n_fingerprints]
tag 3  unknown: the filter could not be built                          may_contain → None
```

**Keys.** The key of a value is `xxhash_rust::xxh3::xxh3_64(bytes)`, where `bytes` comes from `key_bytes` (step 2). Writers collect the hashes, **sort and deduplicate** them, and build from the deduplicated list. This is mandatory: xorf `debug_assert`s that keys are distinct.

**Building.** Counts below are distinct hashes.

| Distinct hashes | Tag |
|---|---|
| 0 | 0 |
| 1, with the value ≤ 1024 bytes | 1 |
| 1, with a longer value | 2 |
| 2 or more | 2 |

For tag 2, call `BinaryFuse8::try_from(&hashes[..])`. On `Err`, write tag 3. The writer always writes `n_fingerprints = fingerprints.len()`, which for xorf's builder equals `segment_count_length + 2 × segment_length`.

**Reading** (`KeyFilter::decode(bytes, offset)`). Any failure below is `Corrupt { what: "key filter", offset }`.
- An unknown tag.
- Bytes after the body. Tags 0 and 3 must be exactly 1 byte.
- Tag 1 with `len > 1024`.
- Tag 2, unless all of these hold:
  - `segment_length` is a power of two and ≥ 1;
  - `segment_length_mask == segment_length − 1`;
  - `segment_count_length ≥ segment_length`, and is a multiple of `segment_length`;
  - `n_fingerprints == segment_count_length + 2 × segment_length`, computed in `u64`;
  - `n_fingerprints` ≤ the remaining bytes. `c.bytes(..)` checks this before any allocation.

These conditions keep xorf's `contains` in bounds: `h0 ≤ scl − 1`, `h1 ≤ scl + sl − 1`, and `h2 ≤ scl + 2·sl − 1`. `scl ≥ sl` excludes the forged geometry `scl = 0`, which would make `contains` panic. Build the filter as `BinaryFuse8 { descriptor: Descriptor { seed, segment_length, segment_length_mask, segment_count_length }, fingerprints: bytes.to_vec().into_boxed_slice() }`.

```rust
pub(crate) enum KeyFilter { Empty, One(Vec<u8>), Fuse(BinaryFuse8), Unknown }
impl KeyFilter {
    pub(crate) fn build(values: &[Vec<u8>]) -> KeyFilter;   // the flattened `Some`s of key_bytes, any order, may repeat
    pub(crate) fn encode(&self, out: &mut Vec<u8>);
    pub(crate) fn decode(bytes: &[u8], offset: u64) -> Result<KeyFilter>;
    pub(crate) fn may_contain(&self, value: &[u8]) -> Option<bool>;
}
```

`may_contain` returns:
- `Empty` → `Some(false)`;
- `One(v)` → `Some(v == value)`;
- `Fuse(f)` → `Some(f.contains(&xxh3_64(value)))`;
- `Unknown` → `None`.

### Steps

1. **`config.rs`.** Add, with its doc comment, and set `Config::default()` to `vec!["user_id".into(), "session_id".into()]`:

   ```rust
   /// Columns, besides the first sort-key column, that get a key filter. Names absent from the
   /// schema, or of an unsupported type, are ignored. Default `["user_id", "session_id"]`.
   pub key_filter_columns: Vec<String>,
   ```

2. **`index.rs`.** Add `KeyFilter` and `pub(crate) fn key_bytes(array: &dyn Array) -> Option<Vec<Option<Vec<u8>>>>`, which gives the bytes to hash per row (`None` for null rows):

   | Type | Bytes |
   |---|---|
   | FixedSizeBinary | the value |
   | Utf8 | the UTF-8 bytes |
   | Int8–Int64, UInt8–UInt32, Timestamp, Date32, Date64 | `arrow::compute::cast(array, &DataType::Int64)`, then `as_primitive_opt::<Int64Type>()` and each value's 8 little-endian bytes. A cast error or a failed downcast returns `None`; never use `as_primitive`, which panics. |

   Any other type, UInt64 included, returns `None`, and the column gets no filter. Document on `StrataFile::may_contain` that integer and timestamp values are passed as 8-byte little-endian `i64`.

3. **Hot tail types** (`hot.rs`).
   - Replace `HotTail.key_filter_index: Vec<u8>` with `key_filters: Vec<(u32, SegmentRef)>`.
   - `encode` writes section 6 as `u32 n · n × (u32 column_id · u64 offset · u32 len)`. Section 6 is **never empty**: a file with no filter writes `u32 0`.
   - `decode` requires:
     - length ≥ 4, and length == `4 + 16 × n`;
     - each `column_id` below `schema.fields().len()`;
     - no repeated `column_id`.

     Otherwise `Corrupt { what: "key filter index" }`.
   - Section 4 (`sort_key_filter: Vec<u8>`) stays bytes.
   - Update every `HotTail { .. }` literal in the `hot.rs` tests and in `writer.rs`: `key_filters: Vec::new()`. The round-trip tests now encode `u32 0` for section 6.

4. **Postscript** (`hot.rs`). The file must record its sort-key column for `lookup`.
   - Postscript byte 32, the first 4 of the 28 reserved bytes, holds `u32 sort_key_column` (`u32::MAX` = none). The reserved area becomes 24 bytes, and decode requires them to be zero. The postscript stays 64 bytes.
   - Update `Postscript` encode, decode, the struct and both `Postscript { .. }` literals in the `hot.rs` tests.
   - Update the S1 postscript line in `prototype-plan.md`: `… · u16 version · u16 flags · u32 sort_key_column · [u8; 24] zero · "STRA"`, and the field-size sum.

5. **Writer** (`writer.rs` `finish`).
   - Set `postscript.sort_key_column` to the first sort-key column's index.
   - **Sort-key filter.** Inside the column loop, at `index == first_sort_column`, build the `KeyFilter` from `key_bytes(array)` (no new concatenation). Store its encoded body in `HotTail::sort_key_filter`. If the sort key's type is unsupported by `key_bytes`, section 4 stays empty.
   - **Other filters.** Iterate the schema in order. A column gets a section-6 filter only when **all** of these hold:
     - its name is in `config.key_filter_columns`;
     - it is not the first sort-key column;
     - it is not a payload column;
     - it is not a map;
     - `key_bytes` supports its type.

     Write its body as a `Kind::KeyFilter` segment (`Codec::Zstd`, through `codec`), and push `(column_id, SegmentRef)`. A duplicate name in the config is harmless.
   - Build the section-6 filter inside the existing column loop, after the column has been written, using the same `array`; never concatenate again. Filters are built by the caller of the column-writing code. M3.2b's `write_narrow` never builds one, so map children and residuals never get a filter.
   - `ColumnSummary` gains `/// Bytes of the column's key filter (section-4 body or key-filter segment), not included in `bytes`.` `pub filter_bytes: u64` (0 when none). Update both `ColumnSummary { .. }` literals.

6. **Open and validation** (`file.rs`). `StrataFile` gains:

   ```rust
   sort_key: Option<usize>,             // from postscript.sort_key_column
   sort_key_filter: Option<KeyFilter>,  // section 4, decoded at open; None if the section is empty
   sort_key_entries: Vec<BlockEntry>,   // section 3, decoded at open
   key_filters: Mutex<HashMap<u32, Arc<KeyFilter>>>,
   ```

   `open` validates the following **after** the existing directory loop, so `directory_ids_layers_and_dictionary_flags_are_checked` still sees `directory column id` and `directory layer` first (M3.2b's layer and layout checks also run after that loop's id check):
   - `sort_key_column` is `u32::MAX` or below `schema.fields().len()`, and that column's directory layer is `Narrow`. Otherwise `Corrupt { what: "sort key column" }`.
   - Section 4, when non-empty, through `KeyFilter::decode(bytes, postscript.hot_offset)`.
   - Section 3, when `sort_key` is set:
     - the entries are contiguous from row 0 (`first_row ==` the previous end, `n_rows > 0`);
     - their total equals `row_count`;
     - their count equals `directory[sort].n_chunks`;
     - each entry with a segment passes the same range check as `read_segment`.

     Otherwise `Corrupt { what: "sort-key block table" }`. With `sort_key = None`, section 3 must be empty.

7. **`may_contain`** (`file.rs`):

   ```rust
   /// Whether `value` may occur in column `column`. `None`: this column has no filter. Integer and
   /// timestamp values are passed as their 8-byte little-endian `i64`.
   pub async fn may_contain(&self, column: &str, value: &[u8]) -> Result<Option<bool>>;
   ```

   - An unknown column name gives `Schema`.
   - The sort-key column uses `sort_key_filter`, and gives `Ok(None)` when it is `None` (an unsupported type).
   - A column listed in section 6 reads its segment on first use (`read_segment(.., Kind::KeyFilter)`, `KeyFilter::decode(.., ref.offset)`) and caches it.
   - Any other column gives `Ok(None)`.

8. **`lookup`** (`file.rs`, helpers in `index.rs`):

   ```rust
   /// Row ranges whose first sort-key column equals `key`, in row order, adjacent rows merged.
   pub async fn lookup(&self, key: &[u8]) -> Result<Vec<std::ops::Range<u32>>>;
   ```

   1. The first sort-key column must exist, be `FixedSizeBinary(w)` with `1 ≤ w ≤ 16`, and have `key.len() == w`. Otherwise `Unsupported { what: "lookup on this sort key".to_string() }`.
   2. If `sort_key_filter` gives `Some(false)`, return an empty `Vec` with no read.
   3. **Candidates:** every entry of `sort_key_entries` that has no `HAS_ZONE` flag, or with `min[..w] ≤ key ≤ max[..w]`. Compare bytewise through `.get(..w)`, by linear scan. Entries are few; no binary search.
   4. Fetch the candidate chunks concurrently: `read_segment(.., Kind::NarrowChunk)`, or a `None` body for `Const` and `ConstNull`. Decode them with `decode_chunk` and the column's field.
   5. Downcast each result to `FixedSizeBinaryArray`; a failure is `Corrupt { what: "sort-key block table" }`. For each valid row *i* whose value equals `key`, add row `first_row.checked_add(i)` (overflow is `Corrupt`). Merge consecutive rows into ranges.

9. **`lib.rs`.**
   - Add `mod index;`.
   - Update the crate doc: replace "no key filters" and "sections 4, 5 and 6 are empty" with "sort-key lookup and key filters (M3.1); no map shredding yet".

10. **Design doc S5.** Make it match this sub-phase:
    - the tags 0–3;
    - the validity rules;
    - `lookup` by zone candidates;
    - `may_contain` as the only accessor; remove `key_filter(column)`;
    - xorf is a crates.io dependency; remove "not in the local registry".

### Tests

**`index.rs`:**
- **Tags.** `KeyFilter::build` for 0 values, 1 short value, 1 long (2 KiB) value, and 10,000 distinct values gives tags 0, 1, 2 and 2. The tag is the first byte of `encode`, since `BinaryFuse8` has no `PartialEq`.
- **Round trips.** Each round trips through `encode` and `decode`, and `may_contain` is `Some(true)` for every inserted value.
- **False positives.** 10,000 inserted values, with value *i* = `xxh3_128(&(i as u64).to_le_bytes()).to_le_bytes()` (16 bytes). Among 10,000 absent values (*i* = 10,000..20,000), at most 200 give `Some(true)`.
- **Corruption.** Each of these gives `Corrupt`:
  - an unknown tag;
  - tag 0 with a trailing byte;
  - tag 1 with `len = 1025`;
  - tag 2 with:
    - `segment_length = 3`;
    - a wrong mask;
    - `scl = 0, sl = 1, mask = 0, n_fp = 2` (the regression case for the `contains` panic);
    - `scl` not a multiple of `sl`;
    - `n_fp ≠ scl + 2 × sl`;
    - `n_fp` past the end of the input.
- **Never panic:**
  - random bytes passed to `decode`, then `may_contain` on any `Ok` result;
  - a structured fuzz: random descriptors that pass the checks (`sl` = 2^k with k in 0..10, `scl = sl × m` with m in 1..64), random fingerprints, and 1,000 random `may_contain` calls each.
- **`key_bytes`:** an Int32 column, a Timestamp column and a Utf8 column give the documented bytes. UInt64 gives `None`.

**`hot.rs`:**
- Section 6 round trips, including `n = 0`.
- Each of these gives `Corrupt`: an empty section 6, a bad length, a `column_id` at or above the schema length, and a repeated `column_id`.
- The postscript with `sort_key_column` round trips, and a non-zero byte in the new 24-byte reserved area gives `Corrupt`.
- Random-bytes proptests on sections 5 and 6 never panic.

**`file.rs`**, through `StrataWriter`, `InMemory` and `open`, with `testgen::batch(20_000)`: 4,000 distinct trace ids of five rows each; FSB16 chunks hold 2,048 rows, so an id spans at most 2 chunks.
- **Present ids.** `lookup` of `(k as u128).to_be_bytes()` for `k in (0..4000).step_by(97)` returns exactly its rows, compared with a scan of the decoded `trace_id` column.
- **Absent ids.** `lookup` of 1,000 absent ids (*i*-th = `xxh3_128(&(1_000_000 + i as u64).to_le_bytes())`) returns empty. At least 980 of them get `may_contain("trace_id", ..) == Some(false)`.
- **Reads.** Take `before = counting.gets()` after `open`. A present-id `lookup` costs at most 2 more `get`s.
- **Unsupported.** `lookup` with a 15-byte key gives `Unsupported`, and so does `lookup` on a file whose only sort key is `start_time`.
- **`user_id` filter.** A test schema in the `file.rs` tests module, 5,000 rows: `trace_id` FSB16 = `((i / 5) as u128).to_be_bytes()`, and `user_id` Utf8 nullable = `format!("user-{}", i % 500)`, null when `i % 3 == 0`. Every present value gives `Some(true)`; at least 980 of 1,000 `format!("other-{i}")` give `Some(false)`.
- **No filter.** `may_contain` on a column with no filter gives `Ok(None)`; an unknown name gives `Schema`.
- **Zero rows.** On a zero-row file, `lookup` returns empty and `may_contain("trace_id", ..)` is `Some(false)`.
- **Corruption.** Add `fn section_bounds(hot: &[u8]) -> [Range<usize>; 7]` to the `file.rs` tests. It walks the seven `u32`-prefixed sections, starting from `hot_offset_of(bytes)`; use it with the existing `refresh_hot_checksum`-style helpers. Every patch is length-preserving:
  - **Segment checksum:** open the file, take `file.hot.key_filters[0].1` (a `SegmentRef`), flip the byte at `offset + SEGMENT_HEADER_LEN`, write the file back, reopen it, and call `may_contain("user_id", ..)`: it gives `Corrupt`;
  - **Bad geometry:** in the `batch(20_000)` file, overwrite the 4 bytes at `section4.start + 1 + 8` (`segment_length`) with `3u32.to_le_bytes()`, then refresh the hot checksum: `open` gives `Corrupt { what: "key filter" }`;
  - patch the postscript's `sort_key_column` (not checksummed) to `u32::MAX`: `open` gives `Corrupt { what: "sort-key block table" }`, because with no sort key section 3 must be empty (step 6); to `n_fields`: `open` gives `Corrupt`.

**`golden_columns_round_trip`** (new, permanent, `writer.rs`): every column of `wide_batch(20_000)` written and read back equals the input.

**`golden_output_hash`.** The new sections and the postscript field change the bytes.
- Run `golden_columns_round_trip` first; it must pass.
- Then update `GOLDEN` to the new printed value, and say why in the commit body.

**`roundtrip_light.rs`:**
- After the column checks, look up the `trace_id` of every 500th row and compare with a scan.
- Print `filter_bytes` in the per-column table.
- If the 1.35× assertion fails, stop and report; do not raise the limit.

### Done when

```bash
cargo +1.99.1 test
cargo +1.99.1 clippy --all-targets -- -D warnings
cargo +1.99.1 fmt --check
cargo +1.99.1 test --release -- --ignored roundtrip_payload roundtrip_light
```

**Commit:** `strata: add key filters and sort-key lookup (M3.1)`

---

## M3.2a: shred and unshred

**Goal:** pure functions that split a `Map<Utf8, Utf8>` array (the `narrow::is_utf8_map` shape) into promoted-key columns and a residual map, and rebuild it. Nothing is written to files in this sub-phase.

**Edit:** `strata/src/{shred.rs (new),format.rs,lib.rs}`, `docs/research/file-format/prototype-plan.md` (S6 and the Limits bullet).

### Rules

**Promotion.**
- For each key, count the rows where it appears **exactly once with a non-null value**.
- A key is promoted when its byte length is ≤ 1,024 and its count is ≥ `((rows * 5).div_ceil(100)).max(1)`, in integer arithmetic.
- `rows = map.len()`, so null maps count in the denominator.
- At most `MAX_PROMOTED_KEYS = 64` keys are promoted. Rank keys by count descending, then key bytes ascending, and keep the top 64. The result is sorted bytewise.

**Row split.** For each row and each promoted key `k`:
- **Null map:** the child is null.
- **`k` appears exactly once with a non-null value `v`:** the child is `v`, and `k` is removed from the row's residual.
- **Otherwise:** the child is null, and every occurrence of `k` stays in the residual. When `k` is present but was not moved (null value or duplicated), the row counts in `residual_hits[k]`.

**Residual.**
- Same `DataType` as the map.
- Each row's remaining entries in their original order.
- A null map gives a null residual row of zero entries.
- A non-null map with no remaining entry gives an empty, non-null residual row.

### Steps

1. **`format.rs`:** `pub const MAX_PROMOTED_KEYS: usize = 64;` with a doc comment. Add it to the Limits bullet of `prototype-plan.md` § Conventions.

2. **`shred.rs`.**

   ```rust
   pub(crate) struct Shredded {
       pub keys: Vec<Vec<u8>>,          // promoted keys, bytewise order
       pub children: Vec<ArrayRef>,     // one StringArray per key, same length as the map
       pub residual: ArrayRef,          // MapArray with exactly the map's DataType
       pub residual_hits: Vec<u32>,     // per key
   }
   pub(crate) fn shred(map: &MapArray) -> Result<Shredded>;
   pub(crate) fn unshred(map_type: &DataType, keys: &[Vec<u8>], children: &[ArrayRef], residual: &ArrayRef) -> Result<ArrayRef>;
   ```

   **`shred`:**
   - Read entries through `map.value_offsets()` (absolute indices into `map.entries()`), and skip null maps.
   - Build children with `StringBuilder` (`append_null` for null).
   - Build the residual as M1F.2's `decode_map` does:
     - keys and values with `StringBuilder` (a null value is `append_null`);
     - `StructArray::try_new(fields.clone(), vec![keys, values], None)`, where `let DataType::Map(entries_field, sorted) = map.data_type()` and `let DataType::Struct(fields) = entries_field.data_type()`;
     - `MapArray::try_new(Arc::clone(entries_field), OffsetBuffer::new(offsets), structs, nulls, *sorted)`, with the offsets built by checked prefix sum.
   - Errors are `Corrupt { what: "shredded map", offset: 0 }`.

   **`unshred`** rebuilds each row:
   - the residual row is null: a null map;
   - otherwise, the residual row's entries plus every `(key, child value)` whose child is non-null, **stable-sorted by key bytes**.

   It builds the `MapArray` the same way, from `map_type`. It returns `Corrupt { what: "shredded map", offset: 0 }` when:
   - the lengths differ;
   - `children.len() != keys.len()`;
   - a child is not a `StringArray`;
   - a child holds a value for a row whose residual is null.

3. **`lib.rs`:** `#[allow(dead_code)] // used by the writer from M3.2b on` then `mod shred;`. The M3.2b sub-phase removes the attribute.

4. **Design doc S6.** Match it to these rules:
   - promotion counts "exactly once with a non-null value";
   - the ranking and the limit;
   - the residual uses the `MAP` encoding, not `IPC`;
   - `residual_hits` as defined here.

### Tests (`#[cfg(test)]` in `shred.rs`)

**Round trip** (proptest, `with_cases(100)`).
- Maps of 0..300 rows, with:
  - null maps and empty maps;
  - 0..12 entries per row;
  - keys from a 15-key vocabulary;
  - null values, duplicate keys, and multi-byte UTF-8 keys and values.
- Expected value: the input with each row's entries stable-sorted by key bytes.
- `unshred(shred(m))` equals it.

**Promotion:**
- A key present once in 5 of 100 rows is promoted; one in 4 of 100 is not.
- With 70 keys, each in every row, exactly the bytewise-smallest 64 are promoted.
- A key of 1,025 bytes in every row is not promoted.

**Row split:**
- A null-valued promoted key: the child is null, the residual keeps `(k, null)`, and `residual_hits[k] == 1`.
- A key that appears twice in a row: both copies stay in the residual, and `residual_hits` counts the row.

**Edge maps:**
- Every row with the same 3 keys, all promoted: each residual row is empty, and the column round trips.
- No frequent key: zero promoted keys, and it round trips.
- A zero-row map round trips.

**`unshred` errors:** each case of its error list gives `Corrupt`.

### Done when

The M3.1 commands pass.

**Commit:** `strata: add map shredding functions (M3.2a)`

---

## M3.2b: shredded maps in the file

**Goal:** the writer stores every `is_utf8_map` column shredded, and `read_column` rebuilds it.

**Edit:**
- `strata/src/{hot.rs,file.rs,writer.rs,reader.rs,lib.rs}`;
- `strata/tests/roundtrip_light.rs`;
- `docs/research/file-format/prototype-plan.md` (S2, only if something below differs).

### Layout

- **Parent.** It keeps its schema position. Directory record:
  - `layer = MapParent` (2), `parent = u32::MAX`, `n_pages = 0`, `n_chunks = 0`;
  - `pages_offset` = the section-7 byte length at that moment;
  - `dict_offset = 0`, `dict_len = 0`, `payload_offset = 0`, `payload_len = 0`;
  - `null_count` = the map's null count.
- **Children and residuals.** They are **appended** after the top-level records. For each shredded map in schema order: its children (promoted keys in bytewise order), then its residual.
  - A child is `layer = MapChild` (3), `parent` = the map's id, written as a narrow Utf8 column (dictionary, chunks, pages, section 7).
  - A residual is `layer = Residual` (4), `parent` = the map's id, written as a narrow column of the map type (`MAP` encoding).
- **`Postscript::n_columns`** is `directory.len()`, children and residuals included, not the schema length.
- **Hot-tail section 5:**

  ```
  u32 n_maps
  per map: u32 map_column_id · u32 residual_column_id · u32 n_keys
           · n_keys × (u32 child_column_id · u32 residual_hits · u16 key_len · key bytes)
  ```

  - Maps appear in schema order, keys in bytewise order.
  - A file with no shredded map writes an **empty** section 5 (length 0). Decode accepts length 0 as zero maps; any other length below 4 is `Corrupt { what: "map layout" }`.
- A map with zero promoted keys still gets a parent, a residual and a section-5 entry with `n_keys = 0`.

### Steps

1. **Hot tail** (`hot.rs`).
   - Replace `HotTail.map_keys: Vec<u8>` with `maps: Vec<MapLayout>`:

     ```rust
     pub(crate) struct MapLayout { pub map: u32, pub residual: u32, pub keys: Vec<MapKey> }
     pub(crate) struct MapKey { pub child: u32, pub residual_hits: u32, pub key: Vec<u8> }
     ```

   - `encode` and `decode` own the layout. `decode` never calls `Vec::with_capacity(n)` from a decoded count without first checking `n × min_entry_len ≤ remaining` (12 bytes per map, 10 per key). It also checks:
     - `n_keys ≤ MAX_PROMOTED_KEYS`;
     - `key_len ≤ 1024`;
     - keys strictly increasing bytewise and valid UTF-8;
     - the section fully consumed.

     Otherwise `Corrupt { what: "map layout" }`. Ids are checked in `open`.
   - Update every `HotTail { .. }` literal: `maps: Vec::new()`.

2. **Open checks** (`file.rs`). Replace the M1F layer check with these rules:
   - **Bad layer, `Corrupt { what: "directory layer" }`** (the existing test `directory_ids_layers_and_dictionary_flags_are_checked` stays as it is):
     - a top-level record (`i < n_fields`) whose layer is not `Narrow`, `Payload` or `MapParent`;
     - a `MapParent` on a field that is not `is_utf8_map`;
     - a record at `i ≥ n_fields` whose layer is not `MapChild` or `Residual`.
   - **Bad layout, `Corrupt { what: "map layout" }`:**
     - section 5 does not name every `MapParent` exactly once, or names a record that is not a `MapParent`;
     - a child or residual id is not at `≥ n_fields`, does not have the matching layer, or has a `parent` that is not that map;
     - some record at `i ≥ n_fields` is not named by exactly one section-5 entry (no orphan, no double naming).
   - Add `pub(crate) fn field_of(&self, column: usize) -> Result<Field>`. It gives:
     - the schema field for `column < n_fields`;
     - `Field::new("", DataType::Utf8, true)` for a `MapChild`;
     - the parent's schema field for a `Residual`.

     `read_narrow` (step 4) and `lookup` use it. `block_entries` is unchanged, and is never called on a `MapParent`.

3. **Writer** (`writer.rs`).
   - **Extract the narrow branch** of `finish`'s column loop into:

     ```rust
     fn write_narrow(&self, array: &ArrayRef, column_id: u32, codec: &mut SegmentCodec,
                     out: &mut Out, pages: &mut Out)
         -> Result<(DirectoryRecord, ColumnSummary, Vec<BlockEntry>)>
     ```

     - It returns a `Narrow` record with `parent = u32::MAX`. For children and residuals, the caller overwrites `record.layer` (`Layer::MapChild` or `Layer::Residual`) and `record.parent` (the map's id) before pushing it. This keeps the function within clippy's `too_many_arguments` limit.
     - The top-level narrow path calls it unchanged. The golden hash must not change from this refactor alone: check it before the next bullet.
   - **Shredded maps.** For a column that `is_utf8_map`:
     - call `shred`;
     - push the parent record (as in § Layout);
     - keep the `Shredded`;
     - do not write the column.
   - After all top-level columns, for each kept map in schema order:
     - call `write_narrow` for each child (`layer = MapChild`), then for the residual (`layer = Residual`), with `column_id` = the next directory index;
     - push their records;
     - build its `MapLayout`.

     Children and residuals never go through `is_payload`: a long-valued child stays narrow (`VARBIN`).
   - **Postscript:** `n_columns = directory.len()`.
   - **`ColumnSummary`:**
     - add `/// Number of promoted keys of a shredded map; 0 otherwise.` `pub shredded_keys: u32`;
     - for the parent row, `chunks` is the sum over its children and residual, `encoding_counts` the element-wise sum, `bytes` the sum, and `shredded_keys` the promoted count;
     - `FileSummary.columns` keeps one row per schema column;
     - update both `ColumnSummary { .. }` literals.
   - Key filters (M3.1) already skip maps.

4. **Reader** (`reader.rs`).
   - Split the narrow body of `read_column` into `async fn read_narrow(file: &StrataFile, column_id: usize, field: &Field) -> Result<ArrayRef>`, which works for any directory id.
   - `read_column(file, column)`:
     - accepts only `column < schema.fields().len()`; anything else is `Schema`;
     - on a `MapParent`, finds its `MapLayout`, reads every child (`field_of`) and the residual concurrently with `read_narrow`, and calls `unshred(field.data_type(), keys, children, residual)`.

5. **`lib.rs` and docs:**
   - Remove the `#[allow(dead_code)]` on `mod shred;`.
   - Update the `Layer::Residual` doc in `hot.rs` ("`IPC` encoding" becomes "`MAP` encoding"), the `hot.rs` module doc, and the `file.rs` test comment "which a schema column may not have in M1".
   - Update the crate doc: "maps are shredded (M3.2)".

6. **Existing tests to update:**
   - `file.rs::empty_file_opens` asserts `directory().len() == spans_schema().fields().len() + 3`. `spans_schema` has three `Map<Utf8, Utf8>` columns, each with a residual and no key on an empty file.
   - `writer.rs::map_column_counts_its_map_chunks` becomes `map_column_is_shredded`. It asserts that `attrs.shredded_keys == 3` and `encoding_counts[Encoding::Map as usize] > 0`. `wide_batch` writes `for j in 0..i % 4`, so its keys are `k0`, `k1` and `k2` (`k3` does not exist), and each appears in at least 19% of rows, so all three are promoted.
   - `golden_columns_round_trip` must pass. `wide_batch`'s `attrs` entries are already sorted, so `assert_same` works directly. Then update `GOLDEN`, and say why in the commit body.

### Tests

**`file.rs` unit tests** (they have the patch helpers and M3.1's `section_bounds`):
- **Layout of `wide_batch(20_000)`.**
  - `wide_schema` has 15 fields, and `attrs` is field 6. The directory has 19 records: 15 top-level, children 15, 16 and 17 for keys `k0`, `k1` and `k2`, and the residual 18.
  - Section 5 lists map 6 with residual 18 and children 15–17.
  - `read_column(.., 6)` equals the input.
- **Length-preserving patches.** Refresh the hot checksum after each; each gives `Corrupt { what: "map layout" }`:
  - a section-5 `map_column_id` set to 0;
  - a child's directory `parent` changed;
  - the last byte of key `k0` in section 5 set to `b'9'`, so that the keys are out of order.
- **Wrong column.** `read_column` with column 15 gives `Schema`.

**`reader.rs`:** the shredded round trip of a map column with a null-valued promoted key, a duplicated key, null maps and empty maps (built in the test), compared with stable-sorted entries.

**`roundtrip_light.rs`:**
- Every column equals the input. Maps are compared directly, since `llmobs-core` builds maps from `BTreeMap`, so their entries are already sorted.
- Print the per-column table with `shredded_keys`, and print the ratio.
- Keep the 1.35× assertion. If it fails, stop and report.

### Done when

The M3.1 commands pass. The commit body holds the new size table and ratio.

**Commit:** `strata: shred Utf8 maps into key columns and a residual (M3.2b)`

---

## Notes for M4

M4 needs:
- `HotTail.maps` (`MapLayout`: child ids, `residual_hits`) for the `MapKeyEq` predicate;
- `StrataFile::may_contain`, which takes raw bytes.

Both are provided by M3 as written.
