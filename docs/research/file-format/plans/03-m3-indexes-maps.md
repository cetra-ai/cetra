# Strata v0 · M3: key lookup, key filters and shredded maps

Implementation plan for phase 4 of [the prototype design](../prototype-plan.md), § S5 and S6. Two sub-phases, each done by one Haiku agent in one go, in order, each ending with the checks green and one commit.

**Prerequisites:** M2.3 is committed.

**Read first:**
- [`01-m1-container-narrow.md`](01-m1-container-narrow.md): § Versions and § Conventions for every sub-phase. They apply unchanged: build environment (`CARGO_TARGET_DIR=$S/target-strata`), checks, lints, no indexing, no panics, test layout.
- [`02-m2-payload.md`](02-m2-payload.md): M2.2 § Rules for every step (error offsets, no indexing, test-module `allow`s).
- [`../prototype-plan.md`](../prototype-plan.md): § S1 (hot-tail sections 4, 5 and 6), § S2 (directory, layers 2, 3 and 4), § S5 and § S6.
- The code in `docs/research/file-format/strata/src/`: `hot.rs`, `file.rs`, `writer.rs`, `reader.rs`, `narrow/`, `format.rs`.

| Sub-phase | Content |
|---|---|
| M3.1 | key filters (binary fuse) and sort-key lookup |
| M3.2 | shredded `Map<Utf8, Utf8>` columns: promoted keys as child columns, a residual map, and reconstruction |

The map-key **predicate** (`MapKeyEq`) and `scan` are M4. M3 only provides the storage and `read_column`.

---

## M3.1: key filters and sort-key lookup

**Goal:**
- **Key filters.** A file can answer "may this value occur in column C?" without reading data, for the first sort-key column and for `user_id` and `session_id`.
- **Lookup.** It can find the row ranges of one sort key by reading only the sort-key chunks that may hold it.

**Edit:**
- `strata/Cargo.toml` (one dependency);
- `strata/src/{config.rs,hot.rs,file.rs,writer.rs,format.rs,lib.rs}`;
- `strata/src/index.rs` (new);
- `strata/tests/roundtrip_light.rs`.

**Dependency.** `xorf = { version = "0.13.0", default-features = false, features = ["binary-fuse"] }`, version looked up with `cargo info xorf` (0.13.0, MIT).

### The key-filter body (`index.rs`)

A key filter is stored as a **body**: in hot-tail section 4 for the sort key, or in a `Kind::KeyFilter` segment for the other columns.

```
u8 tag
tag 0  empty:   the column has no non-null value               may_contain → Some(false)
tag 1  one:     u32 len · value bytes (len ≤ 1024)             may_contain → Some(value == that)
tag 2  fuse:    u64 seed · u32 segment_length · u32 segment_length_mask · u32 segment_count_length
                · u32 n_fingerprints · fingerprints[n_fingerprints]
tag 3  unknown: the filter could not be built                  may_contain → None
```

**Keys.** The key of a value is `xxhash_rust::xxh3::xxh3_64(value bytes)`: the 16 bytes for `FixedSizeBinary(16)`, the UTF-8 bytes for Utf8, and the 8 little-endian bytes for integers and timestamps (as `i64`). Writers sort and deduplicate the keys before building.

**Building.**

| Values (distinct, non-null) | Tag |
|---|---|
| 0 | 0 |
| 1, with byte length ≤ 1024 | 1 |
| 1, longer than 1024 bytes | 2 |
| 2 or more | 2 |

For tag 2, call `xorf::BinaryFuse8::try_from(&keys[..])`. On `Err`, write tag 3.

**Reading tag 2.** The descriptor and fingerprints must satisfy all of these, with checked arithmetic, before `contains` is ever called. Otherwise `Corrupt { what: "key filter" }`.
- `segment_length` is a power of two, `≥ 1`;
- `segment_length_mask == segment_length − 1`;
- `segment_count_length` is a multiple of `segment_length`;
- `segment_count_length + 2 × segment_length ≤ n_fingerprints`, and `n_fingerprints ≤` the remaining bytes (checked before allocating).

These checks are what keep xorf's `contains`, which indexes `fingerprints[h0]`, `[h1]` and `[h2]`, in bounds. Every `h` is below `segment_count_length + 2 × segment_length`. Build the filter as `BinaryFuse8 { descriptor: Descriptor { seed, segment_length, segment_length_mask, segment_count_length }, fingerprints: bytes.into_boxed_slice() }`. Both types are public: `xorf::BinaryFuse8` and `xorf::prelude::bfuse::Descriptor`; check the import path in the crate source.

```rust
pub(crate) enum KeyFilter { Empty, One(Vec<u8>), Fuse(BinaryFuse8), Unknown }
impl KeyFilter {
    pub(crate) fn build(keys: &[(u64, Vec<u8>)]) -> KeyFilter;   // (hash, value bytes), any order, may repeat
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

`Filter` is the xorf trait that provides `contains`; import it.

### Steps

1. **`config.rs`.** Add, with its doc comment:

   ```rust
   /// Columns, besides the first sort-key column, that get a key filter. Names absent from the
   /// schema are ignored. Default `["user_id", "session_id"]`.
   pub key_filter_columns: Vec<String>,
   ```

   `Config` is `Clone`, so a `Vec` is fine.

2. **`index.rs`.** Add `KeyFilter` (above) and `pub(crate) fn key_bytes(array: &dyn Array, row: usize) -> Option<Vec<u8>>`, which returns the bytes to hash for a non-null row of a supported type:
   - FixedSizeBinary: the value;
   - Utf8: the bytes;
   - Int8–Int64, UInt8–UInt32, Timestamp, Date: the `i64` little-endian bytes;
   - null rows and any other type: `None`.

   A column of an unsupported type gets **no** filter at all (no section-6 entry), and `may_contain` returns `Ok(None)` for it.

3. **Writer** (`writer.rs` `finish`). After all columns are written:
   - **Sort key.** Build the `KeyFilter` of the first sort-key column from its concatenated array, and store the encoded body in `HotTail::sort_key_filter` (section 4).
   - **Other columns.** For each name in `config.key_filter_columns` that is in the schema, has a supported type, and is not a payload column:
     - build its filter;
     - write the body as a `Kind::KeyFilter` segment (`Codec::Zstd`, through `codec`);
     - append `u32 column_id · u64 offset · u32 len` to `HotTail::key_filter_index` (section 6).

     The section starts with `u32 n` (the number of entries), as the design's S1 section 6 says. A file with no such column writes `n = 0`.

4. **Hot tail** (`hot.rs`). `HotTail::decode` checks section 6:
   - its length is `4 + 16 × n`;
   - each `column_id` is below the directory length;
   - no `column_id` repeats.

   Otherwise `Corrupt { what: "key filter index" }`. Section 4 is decoded lazily by `file.rs`.

5. **`file.rs`.**
   - At open, decode section 4 with `KeyFilter::decode`. An empty section 4 means "no filter" (`None`), as in M1/M2 files and in files whose first sort key has an unsupported type. Store it in `StrataFile`.
   - Parse section 6 into a `Vec<(u32, SegmentRef)>`.
   - Add a cache `key_filters: Mutex<HashMap<u32, Arc<KeyFilter>>>`.
   - Add:

     ```rust
     /// Whether `value` may occur in the column named `column`. `None`: no filter for this column.
     pub async fn may_contain(&self, column: &str, value: &[u8]) -> Result<Option<bool>>;
     ```

     It resolves `column` by name in the schema. An unknown name is a `Schema` error. For the first sort-key column it uses the section-4 filter; otherwise the section-6 entry, read on first use with `read_segment(.., Kind::KeyFilter)` and cached. With no entry it returns `Ok(None)`.
   - The sort key's column index is needed at read time, but the file does not store which column is the sort key. Add it to the **postscript**, so no other layout changes: postscript byte 32, in the 28 reserved bytes, holds `u32 sort_key_column` (`u32::MAX` = none). The reserved area becomes 24 bytes.
     - Update `Postscript` encode and decode.
     - Update the reserved-bytes check (decode requires the remaining 24 bytes to be zero).
     - Update `prototype-plan.md` S1's postscript line accordingly: `… · u16 version · u16 flags · u32 sort_key_column · [u8; 24] zero · "STRA"`. The total stays 64 bytes.

6. **Lookup** (`index.rs` and `file.rs`):

   ```rust
   /// Row ranges whose first sort-key column equals `key`, in row order, contiguous ranges merged.
   pub async fn lookup(&self, key: &[u8]) -> Result<Vec<std::ops::Range<u32>>>;
   ```

   1. The first sort-key column must be `FixedSizeBinary(w)` with `key.len() == w`; otherwise `Unsupported { what: "lookup on this sort key" }`. A file with no sort key gives the same error.
   2. If the section-4 filter says `Some(false)`, return an empty `Vec` without reading anything.
   3. Decode `hot.sort_key_entries` into `Vec<BlockEntry>`, once at open, and keep it. Each entry with `HAS_ZONE` has `min` and `max` = the first `w` bytes of its zone fields.
   4. **Candidates.** `start = entries.partition_point(|e| zone_max(e) < key)`. Then take entries from `start` while `zone_min(e) ≤ key`. An entry without `HAS_ZONE` (it cannot occur for FSB w ≤ 16, but forged files may have it) is a candidate. Comparisons are bytewise over `w` bytes.
   5. Fetch the candidate chunks concurrently (`read_segment(.., Kind::NarrowChunk)`, or `None` body for `Const`/`ConstNull`) and decode them with `decode_chunk` against the column's field.
   6. For each decoded `FixedSizeBinaryArray`, collect the rows where `value(i) == key` and the slot is valid, as absolute row numbers `first_row + i`, then merge consecutive rows into ranges.

7. **`lib.rs`.** No new re-exports: `lookup` and `may_contain` are methods on the already-public `StrataFile`.

### Tests

**`index.rs`:**
- `KeyFilter::build` for 0, 1 short, 1 long (2 KiB) and 10,000 distinct values gives tags 0, 1, 2 and 2.
- Each round trips through `encode` and `decode`, and `may_contain` is `Some(true)` for every inserted value.
- **False positives:** 10,000 inserted values (random 16-byte ids). Among 10,000 absent random ids, fewer than 100 (1%) give `Some(true)`. Binary fuse 8 has about 0.4% false positives.
- **Corruption:** each of these hand-built fuse bodies gives `Corrupt`:
  - `segment_length = 3` (not a power of two);
  - a wrong mask;
  - `segment_count_length + 2 × segment_length > n_fingerprints`;
  - `n_fingerprints` past the end of the input.
- **Never panic:** random bytes passed to `decode`, then `may_contain` on any `Ok` result, never panic.

**`file.rs`, through `StrataWriter`, `InMemory` and `open`, with `testgen::batch(20_000)`.** Its `trace_id` has 4,000 distinct ids, five rows each.
- `lookup` of every 97th distinct trace id returns exactly the rows with that id, compared with a scan of the decoded `trace_id` column.
- `lookup` of 1,000 absent random ids returns an empty `Vec`. For at least 99% of them, `may_contain("trace_id", ..)` is `Some(false)`.
- `lookup` with a 15-byte key gives `Unsupported`. On a file whose sort key is `start_time` only, `lookup` gives `Unsupported`.
- **Reads:** a lookup reads at most 2 sort-key chunks. Check with the counting store that the number of `get`s after open is at most 2, as the existing tests count them.
- `may_contain("user_id", ..)` on a file built from a batch with a `user_id` Utf8 column (add `user_id` to a test schema: `format!("user-{}", i % 500)`, null when `i % 3 == 0`): `Some(true)` for present values, mostly `Some(false)` for absent ones.
- `may_contain` on a column with no filter gives `Ok(None)`; on an unknown name, `Schema`.
- A zero-row file: `lookup` returns empty, and `may_contain("trace_id", ..)` is `Some(false)`.

**`hot.rs`:**
- Section 6 with a bad length, an out-of-range `column_id`, or a repeated `column_id` gives `Corrupt`.
- The postscript with `sort_key_column` round trips, and a non-zero byte in the new 24-byte reserved area gives `Corrupt`.

**`roundtrip_light.rs`:** after the column checks, look up 100 trace ids (every 500th row's `trace_id`) and compare with a scan. Print the key-filter bytes: the section-4 length and the `user_id` and `session_id` segment lengths.

**`golden_output_hash`.** The writer now emits section 4, section 6 and the sort-key postscript field, so the file bytes change. Update `GOLDEN` **in this commit only**:
- run the test, take the new printed value, and state in the commit body that the hash changed because of the new key-filter sections and the postscript field;
- before updating it, verify that the column data is unchanged: decode every column of `wide_batch(20_000)` from the new file and compare it with the input.

### Done when

```bash
cargo +1.99.1 test
cargo +1.99.1 clippy --all-targets -- -D warnings
cargo +1.99.1 fmt --check
cargo +1.99.1 test --release -- --ignored roundtrip_payload roundtrip_light
```

**Commit:** `strata: add key filters and sort-key lookup (M3.1)`

---

## M3.2: shredded maps

**Goal:** a `Map<Utf8, Utf8>` column (the `is_utf8_map` shape) is stored as:
- one child column per frequent key, as a narrow Utf8 column whose null means "key absent";
- a residual map holding the other keys;
- a parent record that ties them together.

`read_column` rebuilds the original map. This shrinks `resource_attributes`, which is 6.8× Parquet today, because each promoted key becomes a near-constant dictionary column. It also prepares the map-key predicate of M4.

**Edit:**
- `strata/src/{hot.rs,file.rs,writer.rs,reader.rs}`;
- `strata/src/shred.rs` (new);
- `strata/tests/roundtrip_light.rs`;
- `docs/research/file-format/prototype-plan.md` (S6, only if a detail below differs from it).

### Rules

**Which columns.** Every column whose type satisfies `narrow::is_utf8_map` is shredded. Other map types are unchanged and stay narrow (`MAP` or `IPC` encoding).

**Promotion**, per file and per column:
- For each key, count the rows where it appears exactly once with a **non-null** value.
- A key is promoted when that count is at least `ceil(rows × 5 / 100)` and at least 1.
- At most `MAX_PROMOTED_KEYS = 64` keys per map are promoted, a format constant in `format.rs`. Keys are ranked by count, descending, then by key bytes, ascending; the top 64 are promoted.
- The promoted keys are sorted bytewise for storage.

**Row split.** For each row and each promoted key `k`:
- if the map is null, the child is null;
- if `k` appears exactly once with a non-null value `v`, the child is `v`, and `k` is **removed** from the row's residual map;
- otherwise, when `k` is absent, appears more than once, or has a null value, the child is null and every occurrence of `k` stays in the residual. When `k` is present but was not moved, count that row in `residual_hits[k]`.

**Residual.**
- The residual is a column of the same `Map` type, holding each row's remaining entries in their original order.
- A null map gives a null residual.
- A non-null map with no remaining entries gives an empty (non-null) residual.

### Layout

- The parent keeps its schema position. Its directory record has `layer = MapParent` (2), no pages (`n_pages = 0`, `n_chunks = 0`), and `null_count` = the map's null count.
- Children and the residual are **appended** after the top-level columns, in this order: for each shredded map in schema order, its children (promoted keys in bytewise order), then its residual.
  - Child: `layer = MapChild` (3), `parent` = the map's column id. It is written exactly like a narrow Utf8 column, with dictionary, chunks, pages and section 7 entries.
  - Residual: `layer = Residual` (4), `parent` = the map's column id. It is written exactly like a narrow column of the map type (`MAP` encoding).
- Hot-tail section 5, as in the design's S6:

  ```
  u32 n_maps
  per map: u32 map_column_id · u32 residual_column_id · u32 n_keys
           · n_keys × (u32 child_column_id · u32 residual_hits · u16 key_len · key bytes)
  ```

  Maps appear in schema order, keys in bytewise order.
- A shredded map with **zero** promoted keys is still written as a parent, a residual and a section-5 entry with `n_keys = 0`, so readers see one layout.

### Steps

1. **`format.rs`:** `pub const MAX_PROMOTED_KEYS: usize = 64;`. Add it to the design doc's Limits bullet.

2. **`shred.rs`.**

   ```rust
   pub(crate) struct Shredded {
       pub keys: Vec<Vec<u8>>,          // promoted keys, bytewise order
       pub children: Vec<ArrayRef>,     // one StringArray per key, same length as the map
       pub residual: ArrayRef,          // MapArray with the map's exact DataType
       pub residual_hits: Vec<u32>,     // per key
   }
   pub(crate) fn shred(map: &MapArray, rows: usize) -> Result<Shredded>;
   pub(crate) fn unshred(field: &Field, keys: &[Vec<u8>], children: &[ArrayRef], residual: &ArrayRef) -> Result<ArrayRef>;
   ```

   **`shred`** reads entries through `map.value_offsets()` (absolute indices into `map.entries()`), skips null maps, and builds:
   - the children with `StringBuilder`;
   - the residual with `MapBuilder`, its field names taken from the map's `DataType`.

     If `MapBuilder` cannot reproduce the exact `DataType` (field names and nullability), build the `MapArray` with `MapArray::try_new` from the offsets, keys, values and nulls, as M1F.2's `decode_map` does. The residual's `data_type()` must equal the map's.

   **`unshred`** rebuilds each row as:
   - a null map if the residual row is null;
   - otherwise, the union of the residual row's entries and every `(key, child value)` whose child is non-null, sorted bytewise by key, with a stable sort that keeps residual duplicates in their original relative order.

   It builds the `MapArray` with `MapArray::try_new`, using the field's `DataType::Map(entries_field, sorted)` (same names, nullability and `sorted` flag). It returns `Corrupt { what: "shredded map" }` when the lengths differ, or when a child holds a value for a row whose residual is null.

3. **Writer** (`writer.rs` `finish`).
   - For a column that `is_utf8_map`:
     - call `shred`;
     - push the parent directory record;
     - remember the `Shredded`;
     - do **not** write the column itself.
   - After all top-level columns, for each remembered map in schema order:
     - write each child with the existing narrow path (dictionary, chunks, pages), using a `Field::new(<map name> + "\0" + <key>, Utf8, true)` for any per-column logic that needs a field;
     - then write the residual with the narrow path and the map's field;
     - push their directory records (layer 3, then 4) with `column_id` = the next index;
     - append the section-5 entry.
   - `ColumnSummary`:
     - the parent's summary counts the bytes of its children and residual;
     - each child and the residual get no summary row of their own, so `FileSummary.columns` keeps one row per schema column;
     - add `pub shredded_keys: u32` to `ColumnSummary` (0 for other columns), with a doc comment.
   - The key filters of M3.1 ignore map columns: they are not key-filter types.

4. **Hot tail and open.**
   - `HotTail::decode` parses section 5 into `Vec<MapLayout { map: u32, residual: u32, keys: Vec<(u32, u32, Vec<u8>)> }>` and checks:
     - every id is below the directory length;
     - `n_keys ≤ MAX_PROMOTED_KEYS`;
     - keys are strictly increasing bytewise and valid UTF-8;
     - the whole section is consumed.

     Otherwise `Corrupt { what: "map layout" }`.
   - `StrataFile::open` replaces the M1F layer check with:
     - top-level records (`i < n_fields`) have layer `Narrow`, `Payload`, or `MapParent`, and `MapParent` only for a field whose type `is_utf8_map`;
     - records at `i ≥ n_fields` have layer `MapChild` or `Residual`, and a `parent` that is a `MapParent` record;
     - section 5 names every `MapParent` exactly once, and each child and residual id in it has the matching layer and `parent`.

     Any mismatch is `Corrupt { what: "map layout" }`.
   - Add `pub(crate) fn field_of(&self, column: usize) -> Result<Field>`, the field used to decode a column:
     - the schema field for top-level columns;
     - `Field::new("", Utf8, true)` for a `MapChild`;
     - the parent's field for a `Residual`.

     `read_column`, `block_entries` and `decode_chunk` callers use it, so children and residuals decode through the existing narrow path.

5. **Reader** (`reader.rs`).
   - `read_column(file, column)` on a `MapParent` reads every child and the residual with the existing narrow path (an internal `read_narrow(file, column_id, field)`, which is the current narrow body of `read_column`), concurrently, and calls `unshred`.
   - The narrow path must accept column ids `≥ n_fields`. `read_column` itself still takes **schema** column indexes only; ids ≥ `n_fields` are internal.

### Tests

**`shred.rs` round trips** (proptest, `with_cases(100)`): `unshred(shred(m))` equals `m` with each row's entries sorted bytewise by key. Generate maps of 0..300 rows, with:
- null maps and empty maps;
- 0..12 entries per row;
- keys from a 15-key vocabulary, so some cross the 5% line and some don't;
- null values, and duplicate keys within a row;
- multi-byte UTF-8.

The comparison sorts the input rows' entries the same way; the input is built so entries are already sorted except where the test adds duplicates.

**Promotion rules** (unit tests):
- A key in exactly 5% of 100 rows is promoted; one in 4% is not.
- With 70 keys each in every row, exactly 64 are promoted: the bytewise-smallest 64, since counts tie.
- A row where a promoted key has a null value: the child is null, the residual keeps `(k, null)`, and `residual_hits[k] == 1`.
- A row with the key twice: both stay in the residual, and `residual_hits` counts it.
- A map column with no frequent key: zero promoted keys, and it round trips.

**File level** (unit tests in `reader.rs`, through `StrataWriter`, `InMemory` and `open`):
- `testgen::wide_batch(20_000)`'s `attrs` column round trips through `read_column` (compare with sorted entries). The directory has the parent (layer 2), its children and the residual, and section 5 lists them.
- Each corruption gives `Corrupt { what: "map layout" }`:
  - a forged section 5 naming a narrow column as a map;
  - a child whose `parent` is not a map;
  - keys out of order.
- `read_column` with an index ≥ the schema length gives `Schema`.

**`roundtrip_light.rs`:** every column still equals the input. Map rows are compared with sorted entries, which matches the input since `llmobs-core` builds maps from `BTreeMap`. Print the per-column table, which now shows `resource_attributes`, `metadata` and `attributes` with their promoted-key counts. Keep the 1.35× assertion, and print the new ratio.

**`golden_output_hash`.** `wide_batch` has a map column, so the file changes. As in M3.1:
- first verify that every column of the new file decodes equal to the input (with sorted map entries);
- then update `GOLDEN`, and state why in the commit body.

### Done when

The M3.1 commands pass. The commit body holds the new size table and its ratio.

**Commit:** `strata: shred Utf8 maps into key columns and a residual (M3.2)`
