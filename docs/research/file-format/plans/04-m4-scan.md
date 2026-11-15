# Strata v0 · M4: scan

Implementation plan for phases 5a and 5b of [the prototype design](../prototype-plan.md), § S7. Revision 3, after two cold reviews.

There are three sub-phases. Each is done by one Haiku agent in one go, in order, and ends with the checks green and one commit.

**Prerequisites:** M3.2b and M3F are committed.

**Read first:**
- [`01-m1-container-narrow.md`](01-m1-container-narrow.md): § Versions and § Conventions, for every sub-phase.
- [`02-m2-payload.md`](02-m2-payload.md): M2.2 § Rules, for every step.
- [`03-m3-indexes-maps.md`](03-m3-indexes-maps.md): § Global rules.
- [`../prototype-plan.md`](../prototype-plan.md): § S7, covering the reader API, the scan wave by wave, I/O, and dependent waves.
- The code in `docs/research/file-format/strata/src/`:
  - `file.rs`, `reader.rs`, `index.rs`, `shred.rs`, `testgen.rs`;
  - `narrow/{mod,decode,dict}.rs`;
  - `payload/{mod,read,search,frames,refs,header}.rs`;
  - `segment.rs`, `format.rs`.

| Sub-phase | Content |
|---|---|
| M4.1 | I/O layer: batched fetches with coalescing by scan kind, `IoStats` (requests, bytes, waves), one wave per concurrent read |
| M4.2 | `scan` over narrow columns: bound predicates, zone and dictionary pruning, late materialisation, limit |
| M4.3 | payload and map columns in `scan`: payload point reads, `Contains`, `MapKeyEq`, `lookup_scan`, wave counts |

**Scope.** Predicates are evaluated on **decoded** Arrow arrays with Arrow's compute kernels. Zones and dictionaries prune chunks before any decoding. Evaluating predicates directly on encoded data, FastLanes-style, is out of scope for v0; it is listed in the design's § Risks.

**Rules for every sub-phase** (on top of the inherited ones):
- **Docs.** Every new public item, field and variant has a `///` doc comment. `#![warn(missing_docs)]` plus `-D warnings` makes a missing one fail clippy.
- **Crate doc.** Each sub-phase updates the "current limits" list in the `lib.rs` `//!` header.
- **Tests.** Tests cover the current behaviour only. When a later sub-phase lifts a limit, it deletes or replaces the test of that limit.
- **Small and big tests.** A test that builds the 3,000-trace datagen file goes in `strata/tests/scan_datagen.rs`, marked `#[ignore]`, and runs in release. Every other test runs on `testgen` data in the normal `cargo test`.

---

## M4.1: I/O layer

**Goal:** every segment read goes through one function. That function fetches a set of segments as **one wave**: one batch of concurrent requests, after merging nearby ranges according to the scan kind. It counts requests, bytes and waves. Every place that today issues concurrent reads issues exactly one wave.

**Edit:** `strata/src/{io.rs (new),file.rs,reader.rs,payload/frames.rs,payload/read.rs,payload/search.rs,lib.rs}`.

### Types (`io.rs`)

```rust
/// How reads are coalesced (design S7 § I/O).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanKind {
    /// Point lookups: merge ranges whose gap is below 64 KiB.
    Point,
    /// Scans: merge ranges whose gap is below 1 MiB, without growing a merged range past 16 MiB.
    Scan,
}

/// I/O counters of one `StrataFile` since it was opened (or since `reset_io_stats`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IoStats {
    /// Ranged GET requests issued.
    pub requests: u64,
    /// Bytes fetched by those requests.
    pub bytes: u64,
    /// Waves: sets of requests issued together, each waiting on the previous one.
    pub waves: u64,
}

pub(crate) const POINT_GAP: u64 = 64 * 1024;
pub(crate) const SCAN_GAP: u64 = 1024 * 1024;
pub(crate) const SCAN_MAX_RANGE: u64 = 16 * 1024 * 1024;
```

`lib.rs` adds `mod io;` and `pub use io::{IoStats, ScanKind};`.

### Steps

1. **Coalescing** (pure, `io.rs`):

   ```rust
   /// Merged byte ranges covering `ranges`, plus for each input the index of the merged range
   /// holding it. Inputs may overlap or repeat; output ranges are sorted and disjoint.
   pub(crate) fn coalesce(ranges: &[Range<u64>], kind: ScanKind) -> (Vec<Range<u64>>, Vec<usize>);
   ```

   - Sort the input indices by `(start, end)`.
   - Walk them in that order, keeping a current merged range `[s, e)`. The next input `[a, b)`:
     - **always merges** when `a < e` (it overlaps or is contained);
     - otherwise merges when `a − e ≤ gap` (`POINT_GAP` for `Point`, `SCAN_GAP` for `Scan`) and, for `Scan` only, `max(e, b) − s ≤ SCAN_MAX_RANGE`;
     - otherwise starts a new merged range.

     When it merges, `e = max(e, b)`.
   - A single input larger than `SCAN_MAX_RANGE` is never split.
   - An empty input gives two empty vectors.
   - An empty range (`a == b`) is merged like any other.

2. **Batched fetch** (`file.rs`):

   ```rust
   /// Reads, checks and decodes `segments` as one wave. Returns the decoded bodies in input order.
   pub(crate) async fn fetch(&self, segments: &[(SegmentRef, Kind)], kind: ScanKind) -> Result<Vec<Vec<u8>>>;
   ```

   1. With no segment, return an empty `Vec` and count nothing.
   2. Check each `SegmentRef` with `segment_end(r.offset, r.len, self.postscript.hot_offset)?`, as `read_segment` does today.
   3. `coalesce` the byte ranges `offset .. offset + len`.
   4. Fetch every merged range concurrently: `try_join_all` over `self.store.get_range(&self.path, r.clone())`.
   5. For each input, slice it out of its merged range's bytes. The position is `offset − merged.start`; use checked subtraction and `.get(..)`, and give `Corrupt { what: "segment range", offset }` if the slice falls outside the fetched bytes. Then call `decode_segment(slice, offset, kind)`.
   6. Count: `requests += merged ranges`, `bytes +=` the sum of the merged lengths, `waves += 1`.

   **Counters.**
   - Add `requests`, `bytes` and `waves` as `AtomicU64` fields of `StrataFile`, updated with `Ordering::Relaxed`, and add them to the struct literal in `open`.
   - `pub fn io_stats(&self) -> IoStats` reads them, and `pub fn reset_io_stats(&self)` zeroes them.
   - `open`'s own reads are not counted, so the counters start at 0 after `open`.

3. **`read_segment`** keeps its signature, and becomes `self.fetch(&[(*r, kind)], ScanKind::Point)` taking the single body. If the `Vec` does not hold exactly one body, that is `Corrupt { what: "segment range", offset: r.offset }`.

4. **Concurrent reads become one wave.** Every place that runs `try_join_all` over `read_segment` calls now makes **one** `fetch` call, then decodes the bodies in the same order. The call sites, which `grep -rn "try_join_all" src/` finds:

   | Site | Segments of the wave | Kind |
   |---|---|---|
   | `file.rs` `block_entries` | the column's block-table pages | `Point` |
   | `file.rs` `lookup` | the candidate sort-key chunks with `segment_len > 0` | `Point` |
   | `reader.rs` `read_narrow` | the column's chunks with `segment_len > 0` | `Scan` |
   | `payload/read.rs` `read_rows` | all refs pages | `Scan` |
   | `payload/read.rs` `read_payload_column` | all refs pages **and** all frames | `Scan` |
   | `payload/search.rs` `payload_contains`, candidate frames | the candidate frames (`read_frame`, line ~407) | `Point` |
   | `payload/search.rs` `payload_contains`, edge groups | the edge groups needed (`read_edge_group`, line ~424) | `Point` |

   - **`read_payload_column`.** Build one segment list (all refs pages, then all frames) and make one `fetch`. Split the bodies. The page decoding moves out of `read_rows` into `fn rows_from_pages(header: &PayloadHeader, bodies: &[Vec<u8>], at: u64) -> Result<Vec<Option<Vec<u32>>>>`, and `read_rows` becomes `fetch` + `rows_from_pages`. `payload_contains` keeps calling `read_rows`, with `Scan`.
   - **`read_frame` and `read_edge_group`.** Each becomes a pure decode of an already-fetched body, `fn decode_fetched_frame(entry, raw) -> Result<Decoded>` and the same for edges. The caller makes one `fetch` for all candidate frames, then one for all edge groups.
   - **Constant chunks.** Chunks with `segment_len == 0` (`Const`, `ConstNull`) stay out of the segment list, and are decoded with `None`, as today.
   - **Unchanged site.** `reader.rs` `read_shredded` (the `try_join_all` over independent `read_narrow` calls, line ~70) stays as it is: each part counts its own waves, and the `reader.rs` import stays.
   - **Shared decode.** `decode_fetched_frame` (with the `raw_len` check) goes in `payload/frames.rs` as `pub(crate)`, because `read.rs` needs it too (M4.3).
   - Remove the `try_join_all` imports that become unused elsewhere. Behaviour is unchanged, and every existing test stays green.

### Tests (`io.rs` and `file.rs`)

**`coalesce`:**
- `Point`: `[0..10, 100..110]` gives `[0..110]`; `[0..10, 70_000..70_010]` gives two ranges.
- `Scan`: `[0..10, 500_000..500_010]` gives one range. `[0..10 MiB, (10 MiB + 1)..(20 MiB + 1)]` gives two ranges, because of the cap. `[0..20 MiB]` stays one range. `[0..20 MiB, 5..10]` gives one range, with both inputs mapped to index 0.
- Repeated inputs `[5..9, 5..9]` give one range, both mapped to 0.
- An empty input gives empty outputs.
- **Property** (`with_cases(300)`, up to 20 random ranges with starts below 64 MiB and lengths below 8 MiB, for both kinds):
  - every input lies inside its merged range;
  - merged ranges are sorted and disjoint;
  - for `Scan` only, when the inputs are pairwise disjoint, no merged range exceeds `SCAN_MAX_RANGE` unless it equals a single input.

**`fetch`** on a file from `testgen::batch(20_000)`, using `input_tokens`, which has 5 chunks of 4,096 rows:
- Fetching its 5 chunk segments in one `fetch` gives the same bodies as 5 `read_segment` calls.
- After `reset_io_stats`, one `fetch` shows `waves == 1`, `requests ≤ 5` and `bytes ≥` the sum of their lengths.
- `reset_io_stats` zeroes the counters.
- A forged `SegmentRef` whose offset is `hot_offset` gives `Corrupt`.

**One wave per concurrent read** (each on a freshly opened file, after `reset_io_stats`):
- `read_column` of `model`, a dictionary column, in `batch(20_000)`: `waves == 3` (block pages, dictionary, chunks).
- `read_column` of `input_tokens`: `waves == 2` (block pages, chunks).
- `read_column` of the `blob` column of `wide_batch(2_000)` written with `Config { payload_min_avg: 0, ..Config::default() }`, which makes `blob` a payload column (check `directory()[i].layer == Layer::Payload` first): `waves == 2` (header; refs pages and frames).
- `lookup` of the `trace_id` of row 1,000 of `batch(20_000)`: `waves == 1`.

### Done when

```bash
cargo +1.99.1 test
cargo +1.99.1 clippy --all-targets -- -D warnings
cargo +1.99.1 fmt --check
cargo +1.99.1 test --release -- --ignored roundtrip_payload roundtrip_light
```

**Commit:** `strata: add batched, coalesced segment fetches and I/O counters (M4.1)`

---

## M4.2: narrow scan

**Goal:** `StrataFile::scan` returns the rows matching a predicate over narrow columns. It:
- prunes chunks with zones and dictionaries;
- decodes only the predicate columns' surviving chunks;
- fetches the projection only for chunks that hold a selected row;
- honours a row-range restriction and a limit.

Payload and map columns (in the projection or the predicate), `Contains` and `MapKeyEq` give `Unsupported` until M4.3.

**Edit:**
- `strata/src/{scan.rs (new),predicate.rs (new),file.rs,reader.rs,narrow/dict.rs,lib.rs}`;
- `docs/research/file-format/prototype-plan.md` § S7.

### Public types (`predicate.rs`)

```rust
/// A literal in a predicate.
#[derive(Debug, Clone, PartialEq)]
pub enum Lit {
    /// SQL NULL.
    Null,
    /// A boolean.
    Bool(bool),
    /// A signed integer; also a timestamp in the column's unit.
    Int(i64),
    /// An unsigned integer.
    UInt(u64),
    /// A floating-point number.
    Float(f64),
    /// A string.
    Utf8(String),
    /// Bytes, for Binary and FixedSizeBinary columns.
    Bytes(Vec<u8>),
}

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp { /* Eq, NotEq, Lt, LtEq, Gt, GtEq, each with a `///` line */ }

/// A boolean predicate over the columns of one file. Columns are schema indexes.
#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    /// All of the terms; TRUE when empty.
    And(Vec<Predicate>),
    /// Any of the terms; FALSE when empty.
    Or(Vec<Predicate>),
    /// The negation.
    Not(Box<Predicate>),
    /// `column op literal`.
    Cmp(usize, CmpOp, Lit),
    /// `column IN (literals)`.
    In(usize, Vec<Lit>),
    /// `column IS NULL`.
    IsNull(usize),
    /// `column IS NOT NULL`.
    IsNotNull(usize),
    /// `map_column['key'] = literal` (M4.3).
    MapKeyEq(usize, String, Lit),
    /// The payload column's value contains the bytes (M4.3).
    Contains(usize, Vec<u8>),
}
```

`lib.rs` adds `mod predicate;` and `pub use predicate::{CmpOp, Lit, Predicate};`.

### Semantics: SQL three-valued logic

- `Cmp` is NULL when the column value is NULL or the literal is `Lit::Null`.
- `In(c, lits)` is the `Or` of `Cmp(c, Eq, lit)` over `lits`. `In(c, [])` is FALSE for every row, null rows included.
- `IsNull` and `IsNotNull` are never NULL.
- `And` and `Or` follow Kleene logic, and `Not(NULL)` is NULL.
- A row is selected only when the predicate is TRUE.

### Binding (`predicate.rs`)

The predicate is checked and its literals are converted **once, before any wave**. Errors therefore never depend on pruning.

```rust
/// A predicate checked against a file, with its literals converted to the column types.
pub(crate) enum Bound {
    And(Vec<Bound>),
    Or(Vec<Bound>),
    Not(Box<Bound>),
    /// Directory id, operator, literal as a one-element array of the column type (or a
    /// one-element null array for `Lit::Null`).
    Cmp(usize, CmpOp, ArrayRef),
    IsNull(usize),
    IsNotNull(usize),
}

pub(crate) fn bind(p: &Predicate, file: &StrataFile) -> Result<Bound>;
```

**Rules of `bind`:**
- **Column index.** Every column index must be `< schema.fields().len()`; otherwise `Schema { reason: "no column N" }`.
- **Layer.** The column's directory record must have `layer == Narrow`; otherwise `Unsupported { what: "predicate on column N" }`. M4.3 changes this rule.
- **`In`.** `In(c, lits)` becomes `Or` of `Cmp(c, Eq, ..)`, so `In(c, [])` becomes `Or([])`.
- **Not yet supported.** `Contains` and `MapKeyEq` give `Unsupported` (until M4.3).
- **Comparable types.** `Cmp` and `In`, with any literal including `Lit::Null`, are allowed only on Boolean, Int8–64, UInt8–64, Float32/64, Timestamp, Date32/64, Utf8, Binary and FixedSizeBinary columns. Any other type (`tags`, which is `List(Utf8)`, Ipc-encoded types) gives `Unsupported { what: "predicate on column N" }`. `IsNull` and `IsNotNull` are allowed on every Narrow column.
- **Casts that fail.** A negative `Lit::Int` on a UInt column, and `Lit::UInt` on Date32 or Date64 (Arrow has no such cast), give `Unsupported`. This is intended.

**Literal conversion.** Allowed pairs; every other pair gives `Unsupported { what: "literal type for column N" }`.

| `Lit` | Column types | Conversion |
|---|---|---|
| `Null` | any | `new_null_array(column_type, 1)` |
| `Bool` | Boolean | `BooleanArray::from(vec![b])` |
| `Int`, `UInt` | Int8–64, UInt8–64, Timestamp(any unit, any tz), Date32, Date64, Float32, Float64 | a one-element `Int64Array` (or `UInt64Array`), cast with `arrow::compute::cast_with_options(&a, column_type, &CastOptions { safe: false, ..Default::default() })`; a cast error (overflow) is `Unsupported` |
| `Float` | Float32, Float64 | a one-element `Float64Array`, cast the same way (`safe: false`) |
| `Utf8` | Utf8 | `StringArray::from(vec![s.as_str()])` |
| `Bytes` | Binary | `BinaryArray::from(vec![b.as_slice()])` |
| `Bytes` | FixedSizeBinary(w) | `FixedSizeBinaryArray::try_from_iter(std::iter::once(b.as_slice()))`, required `b.len() == w`; otherwise `Unsupported` |

### Evaluation (`predicate.rs`)

```rust
/// Evaluates `b` over `rows` rows. `columns` is indexed by directory id and holds the decoded
/// rows of every column `b` references.
pub(crate) fn evaluate(b: &Bound, columns: &[Option<ArrayRef>], rows: usize) -> Result<BooleanArray>;
```

| Bound | Kernel |
|---|---|
| `Cmp` | `arrow::compute::kernels::cmp::{eq, neq, lt, lt_eq, gt, gt_eq}(&column, &Scalar::new(lit.clone()))` |
| `IsNull`, `IsNotNull` | `arrow::compute::{is_null, is_not_null}` |
| `And`, `Or` | fold with `and_kleene` / `or_kleene`, starting from an all-TRUE / all-FALSE `BooleanArray` of `rows` rows |
| `Not` | `arrow::compute::not` |

A referenced column that is `None` gives `Schema { reason: "internal: missing column N" }`.

**Note.** Arrow's float comparisons use the IEEE total order: NaN is greater than every number and equal to itself, and `-0.0 < 0.0`. `scan`'s results follow Arrow, and the pruning rules below are built to agree with it.

### Pruning (`predicate.rs`)

**Conjuncts.** The conjuncts of a bound predicate are:
- the terms of a top-level `And`, flattened through nested `And`s;
- or the predicate itself when it is not an `And`.

Only conjuncts that are `Cmp(c, op, lit)` with `op ≠ NotEq`, or an `Or` whose terms are all `Cmp(c, Eq, lit)` on the same `c` (an `In`), prune column `c`. Any other conjunct prunes nothing.

```rust
/// For a Dict-encoded column, the dictionary codes whose value satisfies every pruning conjunct
/// on `col`. `None` when there is no such conjunct or the column is not dictionary-encoded.
pub(crate) fn matching_codes(b: &Bound, col: usize, dict: Option<&Dictionary>) -> Result<Option<BTreeSet<u32>>>;

/// Whether chunk `entry` of column `col` may hold a row satisfying `b`. `Ok(true)` when unknown.
pub(crate) fn chunk_may_match(b: &Bound, col: usize, entry: &BlockEntry, field: &Field,
                              dict: Option<&Dictionary>, codes: Option<&BTreeSet<u32>>) -> Result<bool>;
```

If clippy's `too_many_arguments` fires (limit 7, so it should not), group `field`, `dict` and `codes` in a `struct ColumnCtx<'a>`.

**Zone formats** (see `narrow/encode.rs`). `min` and `max` are 16-byte arrays, and indexing is denied, so use `split_first_chunk::<N>()`.

| Column | Zone value |
|---|---|
| Integer, timestamp, date | `i64::from_le_bytes` of the first 8 bytes |
| Float32, Float64 | `f64::from_le_bytes` of the first 8 bytes |
| Dict | `u32::from_le_bytes` of the first 4 bytes (a code) |
| FixedSizeBinary(w), w ≤ 16 | the first `w` bytes, zero-padded to 16; follow `index.rs::zone_may_hold` |

**Reading a literal back.** Null literals are handled first (see below) and never read back. To compare a literal with a zone:
- integer, timestamp and date literals: cast the bound one-element array to Int64 with the default safe `cast`; a null result (out of range) means no pruning;
- float literals: cast it to Float64;
- FixedSizeBinary literals: the array's `value(0)`.

- **`matching_codes`.** Build a `StringArray` of all dictionary values, using a new `pub(crate) fn len(&self) -> usize` on `Dictionary` in `narrow/dict.rs` and `str_value(code)` for each code. Evaluate each pruning conjunct on it with the same kernels, `and_kleene` them, and keep the codes whose result is TRUE. It is computed once per column per scan.

**Rules of `chunk_may_match`, per pruning conjunct** (the chunk may match only if every conjunct may match):
- **`Cmp` with a null literal:** the chunk cannot match. In an `Or` of `Eq` terms (an `In`), a null term can never match, and the other terms decide; with only null terms, the chunk cannot match.
- **`ConstNull` chunk:** cannot match.
- **`Const` chunk:** decode its one row with `decode_chunk(&BlockEntry { n_rows: 1, ..*entry }, None, field, dict)`. A Const chunk of a dictionary column keeps its code in the zone, and `decode_chunk` needs `dict` to resolve it. Evaluate the conjunct on the row; the chunk may match only if the result is TRUE (valid and true).
- **Dict chunk** (`entry.encoding == Encoding::Dict`, the column type is Utf8, and `codes` is given): may match only if some code in `[zone min, zone max]` is in the set, tested with `codes.range(min..=max).next().is_some()`. `codes` and this rule never apply to a `List(Utf8)` column, which also has a dictionary.
- **Integer, timestamp or date column with `HAS_ZONE`:** read the literal back as above, and compare it with the zone's `[min, max]`:

  | Conjunct | Prune when |
  |---|---|
  | `Eq v` | `v < min` or `v > max` |
  | `Lt v` | `min ≥ v` |
  | `LtEq v` | `min > v` |
  | `Gt v` | `max ≤ v` |
  | `GtEq v` | `max < v` |
  | `In` | every value would prune as `Eq` |
- **Float column with `HAS_ZONE`:**
  - Prune only when the chunk does **not** have `HAS_NAN`.
  - Read the literal back as above. For Float32, the bound array already holds the value rounded to `f32`.
  - Never prune when the literal is NaN, `0.0` or `-0.0`.
  - Otherwise use the integer table with `f64` comparisons.
- **FixedSizeBinary(w) column with `w ≤ 16` and `HAS_ZONE`:** the integer table, comparing byte slices of `w` bytes lexicographically.
- **Anything else** (no zone, `Varbin`, `Ipc`, `Map`, `Bool`, `ListDict`, `Plain` without zone): may match.

Check `narrow/mod.rs` for the exact names of the encoding variants, the `flags` constants (`HAS_ZONE`, `HAS_NAN`, `HAS_NULLS`) and the zone fields of `BlockEntry`. Use the names that exist; if one of these flags does not exist, stop and report.

### Metadata loading (`file.rs`)

```rust
/// Loads into the caches the block entries and dictionaries of `columns` (directory ids) that
/// are not cached yet, with one `fetch` for all their pages and dictionaries.
pub(crate) async fn load_metadata(&self, columns: &[usize], kind: ScanKind) -> Result<()>;
/// The cached-or-read dictionary of a directory id; `None` for a column without one.
pub(crate) async fn dictionary(&self, column: usize) -> Result<Option<Arc<Dictionary>>>;
```

- **What `load_metadata` collects.** It dedupes `columns`. It skips:
  - `Payload` records;
  - `MapParent` records (M4.3 expands them before the call);
  - columns whose entries are already cached.

  For each remaining column it collects **all** its block-table pages, and its dictionary segment when bit 0 of its flags is set and `n_chunks > 0`. It makes one `fetch`, or none if nothing is left.
- **Decoding.** Move the page-decoding loop of `block_entries` into `fn decode_pages(&self, record: &DirectoryRecord, pages: &[PageRef], bodies: &[Vec<u8>]) -> Result<Vec<BlockEntry>>`, with every existing check. Move the dictionary flag check of `reader.rs` (the `match (record.flags & 1 != 0, record.dict_offset != 0)`) into `load_metadata`.
- **Caches.**
  - Add the field `dicts: Mutex<HashMap<usize, Option<Arc<Dictionary>>>>`.
  - A column with no dictionary, or with `n_chunks == 0`, caches `None`.
  - `block_entries(c)` becomes `load_metadata(&[c], ScanKind::Point)` plus a cache read, and keeps returning an empty `Vec` for `Payload` records. `dictionary(c)` works the same way.
- **`read_narrow`** uses `block_entries`, then `dictionary` (the dictionary only if the entries are not empty). An empty file therefore gains no read.

### `scan` (`scan.rs`)

```rust
/// What to read from a file.
#[derive(Debug, Clone)]
pub struct ScanRequest {
    /// Schema indexes of the output columns, in output order. May be empty.
    pub projection: Vec<usize>,
    /// Rows must satisfy it; `None` selects every row.
    pub predicate: Option<Predicate>,
    /// Restrict the scan to these rows: sorted, disjoint, non-empty ranges within the file.
    pub row_ranges: Option<Vec<Range<u32>>>,
    /// At most this many rows, the first ones in file order.
    pub limit: Option<usize>,
    /// How reads are coalesced.
    pub kind: ScanKind,
}

impl StrataFile {
    /// Rows of the file matching `req`, as record batches of at most 8,192 rows.
    pub async fn scan(&self, req: &ScanRequest) -> Result<Vec<RecordBatch>>;
}
```

`lib.rs` adds `mod scan;` and `pub use scan::ScanRequest;`.

`scan` returns a `Vec`, not a stream: the batches of one file fit in memory in the prototype, and M5 wraps them into a DataFusion stream.

**Algorithm.**

1. **Validate.**
   - Every projection index must be `< schema.fields().len()` (`Schema`), and its record must be `Narrow`; until M4.3, anything else is `Unsupported { what: "projection of column N" }`.
   - `bind` the predicate.
   - Each range in `row_ranges` must have `start < end` and `end ≤ row_count`, and each range must start at or after the previous one's end. Otherwise `Schema { reason: "row ranges" }`.
   - `limit == Some(0)` returns an empty `Vec` with no wave.
2. **Metadata wave.** Call `load_metadata` once, with every predicate and projection column.
3. **Candidate rows.** `scan` works on a sorted list of disjoint candidate row ranges.
   - Start from `row_ranges`, or `[0..row_count]`. With 0 rows, the list is empty.
   - For each predicate column `c` with a pruning conjunct:
     - compute `matching_codes` once;
     - for each chunk of `c` for which `chunk_may_match` is false, remove the chunk's rows `first_row .. first_row + n_rows` from the list with `fn subtract(ranges: &[Range<u32>], remove: Range<u32>) -> Vec<Range<u32>>` (in `scan.rs`). Unit-test it on 4 cases: a split, a trim at either end, a dropped range, and a no-op.
   - `dictionary()` is called only after `load_metadata`, so it never reads.
   - An empty list ends the scan: return an empty `Vec`, with no further wave.
4. **Filter wave** (only with a predicate).
   - **Fetch.** The chunks to fetch are, for every predicate column, the chunks that overlap a candidate range, are not already decoded, and have `segment_len > 0`. Fetch them all in one `fetch`. Decode every overlapping chunk, those of length 0 included, and keep the decoded arrays in a `HashMap<(usize, usize), ArrayRef>` keyed by (directory id, chunk index). Step 6 reuses them.
   - **Evaluate.** For each candidate range:
     - assemble each predicate column's rows by slicing the decoded chunk arrays to the range and concatenating them with `arrow::compute::concat`;
     - evaluate the bound predicate;
     - push `range.start + i` for every row with `mask.is_valid(i) && mask.value(i)` into the selection, a sorted `Vec<u32>`. Kleene and comparison outputs may have the value bit set on a null slot, so the validity check is required.
   - **No predicate:** the selection is every row of the candidate ranges, and there is no filter wave.
5. **Limit.** Truncate the selection to `limit` rows.
6. **Projection wave.**
   - **Fetch.** For every projection column, fetch the chunks holding at least one selected row that are not in the map, in one `fetch`, and decode them.
   - **Take.** Build each output column for a batch of at most 8,192 selected rows:
     - for each chunk holding rows of the batch, call `arrow::compute::take` on the decoded chunk with the chunk-local indexes (`row − first_row`, as a `UInt32Array`);
     - concatenate the pieces.
   - **Output schema.** `Arc::new(self.schema().project(&req.projection)?)`. The Arrow error converts through `StrataError::Arrow`.
   - **Batches.** Build each batch with `RecordBatch::try_new_with_options(schema, columns, &RecordBatchOptions::new().with_row_count(Some(n)))`, so an empty projection works.
   - With no selected row, return an empty `Vec`.
7. **Empty waves.** A wave with nothing to fetch makes no `fetch` call and counts nothing.

**Design doc.** In `prototype-plan.md` § S7, replace the reader API sketch with the real one:
- `scan(&ScanRequest) -> Result<Vec<RecordBatch>>`, with `Predicate` and `Lit` as above;
- `lookup_scan(key, &[usize])` (M4.3);
- `open(store, path, hint, &Config)`.

Add one line: "Returning a `Vec` instead of a stream is a prototype simplification; M5 wraps it."

### Tests (`scan.rs`, over `testgen::wide_batch(20_000)` written with the default config)

**Oracle.**
- Run `read_column` on every column.
- Evaluate the same `Predicate` with Arrow kernels on the full arrays, using the same literal conversion: call `bind`, then `evaluate` with `columns` holding the full arrays and `rows = 20_000`.
- `filter_record_batch` the projected columns with the result.

Each case compares `scan`'s concatenated batches with the oracle: same rows, same order.

**Predicates:** 30 cases.
- `Cmp` with each `CmpOp` on:
  - `input_tokens` (Int64 with nulls), with literals below, inside and above the column's range;
  - `cost` (Float64, with NaN at the multiples of 997 that are not multiples of 11, so every chunk has `HAS_NAN` and none is zone-pruned), with `Gt 100.0`, `Lt 0.5` and `Eq` NaN;
  - `f32` (Float32, no NaN), with `Eq 0.1`, `Gt 5_000.0` and `Lt 0.0`. For `Gt 5_000.0`, also assert that the filter wave fetched fewer than 5 chunks: after `reset_io_stats`, `requests < 5` for the filter wave. Only `f32 > 5,000` (row > 10,000) can match, so chunks 0 and 1 are pruned;
  - `start_time` (Timestamp), with `Lit::Int` nanosecond literals;
  - `trace_id` (FSB16), with `Lit::Bytes` of row 7,000's id;
  - `model` and `status` (dictionary Utf8), with a value present in the column and one absent.
- `In` with 0, 1 and 3 values, one of which is `Lit::Null`.
- `IsNull(model)` and `IsNotNull(cost)`.
- `And` and `Or` of two columns.
- `Not` over a column with nulls.
- `Cmp(input_tokens, Eq, Lit::Null)`.

**M4.1 test to update:** `read_column(model)` now takes `waves == 2`, because `load_metadata` fetches the pages and the dictionary together. `input_tokens` stays at 2.

**Pruning** (`projection = [start_time]`, after `reset_io_stats` on a freshly opened file):
- `Cmp(start_time, GtEq, T)`, where `T` is the value of row 19,000. `start_time` has 5 chunks; only the last one, rows 16,384–19,999, survives. Expect:
  - `waves == 2`: metadata, then filter. The projection is the predicate column, so its chunks are reused and no projection wave runs.
  - `requests == 2`.
  - The result equals the oracle.
- `Cmp(status, Eq, "missing")`, a value absent from the dictionary: `waves == 1` (metadata only) and no batch.

**Rows and limits:**
- `row_ranges = [100..200, 5_000..5_010]` with no predicate returns exactly those 110 rows.
- With a predicate, the result is the intersection of the ranges and the oracle.
- `limit = 7` returns the oracle's first 7 rows. `limit = 0` returns no batch and makes no wave.
- `projection = []` with a predicate returns batches with 0 columns, whose row counts sum to the oracle's count.

**Errors:**
- A projection index of 99 gives `Schema`.
- `row_ranges = [5..9, 0..3]` gives `Schema`, and so does `[5..5]`.
- `Lit::Bytes` of 15 bytes on `trace_id` gives `Unsupported`.
- `Lit::Float(3.5)` on `input_tokens` gives `Unsupported`.
- `Lit::Int(300)` on a column whose type has no room for it gives `Unsupported`. If no Int8 or UInt8 column exists in `wide_batch`, use `Lit::UInt(u64::MAX)` on `input_tokens`.
- `Lit::Utf8` on `input_tokens` gives `Unsupported`.
- `Cmp(attrs, ..)` (a map column) gives `Unsupported`.
- `And([Cmp(status, Eq, "missing"), Cmp(trace_id, Eq, Bytes of 3 bytes)])` gives `Unsupported`, even though the first conjunct prunes everything.

**Never panic** (proptest, `with_cases(64)`):
- Random `Predicate` trees of depth ≤ 3 over the narrow columns of `wide_batch(3_000)`.
- Random literals of every `Lit` variant, including the wrong ones.
- `scan` gives the oracle's rows, or `Err` of the same variant as `bind`.

### Done when

The M4.1 commands pass.

**Commit:** `strata: add narrow scan with predicates and pruning (M4.2)`

---

## M4.3: payload columns, maps and `lookup_scan`

**Goal:** `scan` supports payload columns, shredded map columns, `Contains` and `MapKeyEq`. `lookup_scan` meets design S7's wave counts.

**Edit:**
- `strata/src/{scan.rs,predicate.rs,file.rs,index.rs,payload/read.rs,payload/search.rs,payload/mod.rs,reader.rs,lib.rs}`;
- `strata/tests/scan_datagen.rs` (new);
- `docs/research/file-format/prototype-plan.md` § S7. Update its wave table to this plan's counts:
  - `Contains` terms add up to 4 waves each, instead of joining the filter wave;
  - the lookup waves are those of step 7.

  The metadata wave also covers the headers of `Contains` payload columns and the residuals of `MapKeyEq` masks.

### Steps

1. **Payload header cache** (`payload/read.rs`, `file.rs`).
   - Split `read_header` into a pure `pub(crate) fn decode_header(file: &StrataFile, record: &DirectoryRecord, body: &[u8]) -> Result<PayloadHeader>`, which holds the existing row-count check, plus the fetch.
   - Add the field `payload_headers: Mutex<HashMap<usize, Arc<PayloadHeader>>>` to `StrataFile`, keyed by directory id.
   - `read_header` returns `Arc<PayloadHeader>`, read through the cache. Update its callers (`read_payload_column`, `payload_contains`, and the search tests) to use `&*header` where a reference is needed.
   - `load_metadata` also loads the headers of the `Payload` columns it is given, in the same wave. It no longer skips them.
   - Add `pub(crate) use header::PayloadHeader;` to `payload/mod.rs`. `PayloadHeader`'s fields and types used from `file.rs` become `pub(crate)` as needed.
   - `load_metadata` skips a Payload column whose header is cached. The M4.2 rule "skip columns whose entries are cached" applies only to narrow columns.

2. **Payload point reads, in stages** (`payload/read.rs`). There is no self-fetching function: `scan` and `lookup_scan` drive the stages for **all** payload columns together, so each stage is one wave across columns.

   ```rust
   /// Refs pages holding at least one of `rows` (sorted, unique): their segments and page indexes.
   pub(crate) fn refs_segments(header: &PayloadHeader, rows: &[u32]) -> (Vec<(SegmentRef, Kind)>, Vec<usize>);
   /// The chunk-id lists of `rows`, `None` for a null row, from the fetched bodies of those pages.
   pub(crate) fn ids_of(header: &PayloadHeader, pages: &[usize], bodies: &[Vec<u8>], rows: &[u32], at: u64) -> Result<Vec<Option<Vec<u32>>>>;
   /// Frames holding at least one id of `ids`: their segments and frame indexes.
   pub(crate) fn frame_segments(header: &PayloadHeader, ids: &[Option<Vec<u32>>]) -> Result<(Vec<(SegmentRef, Kind)>, Vec<usize>)>;
   /// Rebuilds the values from the fetched, decoded frames (frame index → raw bytes and chunk ranges).
   pub(crate) fn build_values(header: &PayloadHeader, frames: &HashMap<usize, (Vec<u8>, Vec<Range<usize>>)>, ids: &[Option<Vec<u32>>], field: &Field, at: u64) -> Result<ArrayRef>;
   ```

   - **Page boundaries.** Page `p` holds rows `p × 4,096 ..` (M3F's rule). `ids_of` decodes each page with `decode_page`, checks its row count, and picks the rows. `rows_from_pages` (M4.1) is rewritten as `ids_of` over every row, so pages are decoded in one place.
   - **`build_values`** is the rebuild loop of `read_payload_column`, moved here, with the same size (`MAX_VALUE_BYTES`) and UTF-8 checks and the same frame-length check. Each frame is decoded with `decode_frame` before it goes into the map.
   - **`read_payload_column`** becomes:
     - `refs_segments` of all rows, and the segments of all frames;
     - one `fetch`;
     - decode the frames;
     - `ids_of` and `build_values`.

     One copy of the rebuild code remains.

3. **Projection of payload and map columns** (`scan.rs`).
   - **Map columns.** For each projected `MapParent`, find its `MapLayout` in `hot.maps`. Its child and residual ids are added to the narrow columns of steps 2 and 6, and the metadata wave expands to them.

     After the narrow take, call `unshred(field.data_type(), keys, children, residual)` on the selected rows. `keys` are the promoted keys, in the order `hot.maps` lists them. `children` and `residual` are the taken arrays of the selected rows.
   - **Payload columns.** After the selection is final (step 5), run two more waves:
     - one `fetch` with the `refs_segments` of every projected payload column;
     - then one `fetch` with the `frame_segments` of every column.

     Then call `build_values` per column, over the whole selection, and slice the result into the output batches.
   - **Predicates on these columns.** `Cmp`, `In`, `IsNull` and `IsNotNull` on a payload or `MapParent` column stay `Unsupported` at `bind`.

4. **`Contains`** (`predicate.rs`, `scan.rs`, `payload/search.rs`, `payload/mod.rs`).
   - **Masks arrive here.** This step adds, together:
     - the variant `/// A precomputed per-row mask. Mask(usize)` to `Bound`;
     - the `masks: &[BooleanArray]` parameter to `evaluate`, after `columns`, with the row `| Mask(i) | masks.get(i), else Schema { reason: "internal: missing mask N" } |`;
     - the `bind` signature change below.

     Update M4.2's callers and tests (they pass `&[]`).
   - **Binding.** `bind` accepts `Contains(c, needle)` only on a `Payload` column with `needle.len() ≤ MAX_NEEDLE`; otherwise `Unsupported`. It becomes a `Mask(i)` leaf, and `bind` returns the list of mask sources with the `Bound`. Change the signature to `bind(..) -> Result<(Bound, Vec<MaskSource>)>`, with `pub(crate) enum MaskSource { Contains(usize, Vec<u8>), MapKey(usize, String, ArrayRef) }`.
   - **Evaluation.** `scan` computes each `Contains` mask before the filter wave, with `payload_contains(file, record, needle)`. That gives a full-column `rows: Vec<bool>`, turned into a `BooleanArray` with no nulls. For each candidate range, the mask is sliced to the range.
   - **Null rows.** `Contains` is never NULL: a null row is FALSE, so `Not(Contains(..))` selects null rows. This deviates from SQL's three-valued logic. Document it on `Predicate::Contains`, and test it: `Not(Contains(blob, ..))` selects the rows where `blob` is null.
   - **Waves.** `payload_contains` keeps its own reads: filters (1 wave), candidate frames (1), edge groups (1) and refs pages (1). The `scan` doc comment says "each `Contains` term adds up to 4 waves".
   - **Clean-up.**
     - Remove the `#[allow(dead_code)]` on `payload_contains` and on `ContainsResult`. Keep `#[cfg_attr(not(test), allow(dead_code))]` on the fields `frames_read` and `edge_groups_read`, which only tests read.
     - Add `pub(crate) use search::{payload_contains, ContainsResult};` to `payload/mod.rs`. Re-export the step 2 functions the same way.

5. **`MapKeyEq`** (`predicate.rs`, `scan.rs`).

   **Semantics.** `MapKeyEq(c, k, lit)` is `value = lit`, where `value` is the single non-null value of key `k` in the row's map. `value` is NULL if:
   - the map is null;
   - the key is absent;
   - its value is null;
   - or the key occurs more than once.

   `Lit::Null` gives NULL for every row. The literal must be `Lit::Utf8` or `Lit::Null`; otherwise `Unsupported`. `c` must be a `MapParent`; otherwise `Unsupported`.

   **Binding.**
   - **`k` is a promoted key** (in the column's `MapLayout.keys`): bind `Cmp(child_id, Eq, lit)` directly, with `child_id` the key's child id. Shredding moves a key to its child only when it occurs exactly once with a non-null value, and removes it from the residual. The child is therefore exactly `value`, whatever `residual_hits` is. The child is pruned by its zones and dictionary like any narrow column.
   - **Otherwise:** a `Mask(i)` leaf with `MaskSource::MapKey(residual_id, k, lit_array)`. The residual id joins the predicate columns of the metadata and filter waves.

     For each candidate range, the mask is computed from the residual's decoded rows. For each row:
     - NULL if the map row is null;
     - otherwise the entries with key `k` are looked up: with exactly one entry whose value is non-null, the result is `value == lit`; in every other case it is NULL.

     The mask is computed per range in `scan`. It is passed to `evaluate` through `masks`, which therefore holds, for `MapKey` sources, arrays that may contain nulls.

6. **`lookup` split** (`file.rs`).

   ```rust
   /// The sort-key chunks that may hold `key`, or `None` when the sort-key filter or the zones
   /// rule `key` out (no read needed). Errors as `lookup` does for a bad key width.
   pub(crate) fn lookup_candidates(&self, key: &[u8]) -> Result<Option<Vec<BlockEntry>>>;
   /// Row ranges of `key` in the fetched candidate chunks (bodies in candidate order, `None`
   /// for chunks with `segment_len == 0`).
   pub(crate) fn rows_of_key(&self, candidates: &[BlockEntry], bodies: &[Option<Vec<u8>>], key: &[u8]) -> Result<Vec<Range<u32>>>;
   ```

   `lookup` becomes `lookup_candidates` + one `fetch` + `rows_of_key`. An empty candidate list is `None`. `index.rs` changes only if `merge_rows` needs it.

7. **`lookup_scan`** (`scan.rs`):

   ```rust
   /// Rows whose first sort-key column equals `key`, with the projected columns.
   pub async fn lookup_scan(&self, key: &[u8], projection: &[usize]) -> Result<Vec<RecordBatch>>;
   ```

   It uses `ScanKind::Point`. Let *narrow* be the projected narrow columns plus the children and residuals of projected maps, and *payload* the projected payload columns.

   1. `lookup_candidates(key)`. With `None`, return an empty `Vec`: **0 waves**.
   2. Let *span* be the rows from the first candidate's `first_row` to the last candidate's end.
   3. **Cold** (some block entries, dictionary or payload header of these columns is not cached):
      - **W1:** one `fetch` with the candidate sort-key chunks plus everything `load_metadata` would load for these columns. `load_metadata` is rewritten as two functions around a `fetch`, so that the candidate chunks can join the same `fetch`:

        ```rust
        /// What `metadata_segments` asked for: for each item, its directory id and its
        /// positions in the segment list.
        pub(crate) struct MetadataPlan {
            /// Narrow column, its pages, and the positions of their bodies.
            pub entries: Vec<(usize, Vec<PageRef>, Range<usize>)>,
            /// Column with a dictionary, and the position of its body.
            pub dicts: Vec<(usize, usize)>,
            /// Payload column, and the position of its header body.
            pub headers: Vec<(usize, usize)>,
            /// Narrow columns with no dictionary or no chunk, which cache `None`.
            pub no_dict: Vec<usize>,
        }
        pub(crate) fn metadata_segments(&self, columns: &[usize]) -> Result<(Vec<(SegmentRef, Kind)>, MetadataPlan)>;
        pub(crate) fn store_metadata(&self, plan: MetadataPlan, bodies: &[Vec<u8>]) -> Result<()>;
        ```

        **Cold** means `metadata_segments` returned a non-empty list. The positions are offsets into the list the caller passes to `fetch`. When the candidate chunks come first, the caller shifts the bodies slice it passes to `store_metadata`.
      - **Sort-key column in the projection:** W2 does not fetch it again; reuse its W1 bodies.
      - Then `rows_of_key` gives the exact ranges.
      - **W2:** one `fetch` with the narrow chunks overlapping the exact ranges plus the `refs_segments` of the selected rows.
      - **W3:** one `fetch` with the `frame_segments`.
   4. **Warm** (everything cached):
      - **W1:** one `fetch` with:
        - the candidate sort-key chunks;
        - the narrow chunks overlapping *span*;
        - the `refs_segments` of every row of *span*.
      - Then `rows_of_key`, and the fetched chunks and pages are cut to the exact rows.
      - **W2:** one `fetch` with the `frame_segments`.
   5. With no payload column projected, the last wave is skipped:
      - **cold:** 2 waves (3 with open);
      - **warm:** 1.
   6. **Output.** The output is built as in `scan`'s step 6, plus the payload and map steps above.

### Tests

**Unit tests** (`payload/read.rs`), on the `blob` column of `wide_batch(2_000)` written with `payload_min_avg: 0`:
- **Point reads.** For 50 sorted rows `(i × 37) % 2_000` for `i` in `0..50`, sorted and deduplicated, `refs_segments`, `fetch`, `ids_of`, `frame_segments`, `fetch` and `build_values` give those rows of `read_payload_column`.
- **No rows.** Rows `[]` give no segments, and `build_values` gives an empty array.

**`scan` tests** (`scan.rs`, `wide_batch(20_000)`, default config):
- **Promoted key.** `MapKeyEq(attrs, "k0", Utf8("v5-0"))`: compare with the oracle, computed from `read_column(attrs)` by the semantics above. `k0` is promoted. With `projection = [uid]`, expect `waves == 3` (metadata, filter, projection). Also assert that `bind` produced a `Bound::Cmp` on the child id and no `Mask`.
- **Absent key.** `MapKeyEq(attrs, "k9", Utf8("x"))` selects no row, and so does `Not(MapKeyEq(attrs, "k9", ..))`, since NULL is not TRUE.
- **Map projection.** `projection = [attrs, uid]` with `row_ranges = [10..60]` equals the oracle.
- **Payload projection.** On `wide_batch(20_000)` with `payload_min_avg: 0`, `projection = [blob, uid]` with `Cmp(input_tokens, Gt, ..)` equals the oracle.
- **`Contains`.**
  - With `payload_min_avg: 0`, `Contains(blob, [7u8; 4])` equals brute force over `read_column(blob)`.
  - With the default config, `Contains(uid, ..)` gives `Unsupported`.
  - A needle of 65 bytes gives `Unsupported`.
- **Tests that M4.2 wrote for the old limits.** Replace "a payload column in the projection gives `Unsupported`" and the `Contains` and `MapKeyEq` `Unsupported` tests with the cases above.

**`lookup_scan` waves** (`scan.rs`, `batch(20_000)`, projection = every column):
- **Cold.** Open, `reset_io_stats`, then `lookup_scan` of row 1,000's `trace_id`: `waves == 2` (no payload column in `batch`).
- **Warm.** Then `lookup_scan` of row 15,000's `trace_id`, with the same projection: `waves == 1`.
- **Absent key.** `[0xFFu8; 16]` gives 0 waves and no batch. Every `batch` id is `(i / 5)` as a big-endian `u128`, so this key is above every zone; `[0u8; 16]` is present (rows 0 to 4).
- **Results.** Every result equals the oracle: `scan` with `Cmp(trace_id, Eq, ..)`.

**Datagen tests** (`tests/scan_datagen.rs`, `#[ignore]`, release). They build the 3,000-trace file as `roundtrip_payload.rs` does, and look columns up with `schema.index_of`.
- **`scan_datagen_lookup`:**
  - **Cold:** `lookup_scan` of the trace id of row 5,000, every column projected: `waves == 3`.
  - **Warm:** `lookup_scan` of the trace id of row 20,000, same projection: `waves == 2`.
  - **Absent:** `[0u8; 16]` gives `waves == 0`.
  - **Results:** each equals the oracle: `read_column` of every column, filtered on `trace_id`.
  - **Printed:** the cold and warm `waves`, `requests` and `bytes`, plus `file.size()`, as the first G1 data point.
- **`scan_datagen_scan`:**
  - **Payload projection:** `projection = [trace_id, input, output]` with `start_time` in a 10-minute window starting at row 0's `start_time` equals the oracle.
  - **`Contains`:** 20 present needles (substrings of 8 to 32 bytes of random non-null `input` values) and 10 absent ones, each equal to brute force.
  - **Combined:** `And(Contains(input, needle), Cmp(span_kind, Eq, Utf8(<row 0's span_kind>)))` equals the oracle.
- **Preconditions** for both tests: `assert!(file.row_count() > 20_000)`. The lookup rows (5,000 and 20,000) must have a non-null `input`: assert it, and if one does not, take the next row whose `input` is non-null and say so in the report.
- **Map pruning** is an in-crate test, because `hot.maps` is `pub(crate)`. It is `#[ignore]` test `scan_datagen_map_pruning` in `src/scan.rs`, built on `fmt_testdata` (a dev-dependency) the same way.
  - On `resource_attributes`, take its `MapLayout` from `file.hot.maps`.
  - Assert `layout.keys` is not empty, and that the first key's child record has `flags & 1 != 0` (a dictionary). If no promoted key has a dictionary, stop and report.
  - `MapKeyEq(.., key, Utf8("\"nope\""))`: the value is JSON-quoted and absent, so the child's dictionary prunes every chunk, and `waves == 1`.
  - Add `scan_datagen_map_pruning` to the release command.

### Done when

- The M4.1 commands pass, with the release command extended to `cargo +1.99.1 test --release -- --ignored roundtrip_payload roundtrip_light scan_datagen --nocapture`. The `scan_datagen` filter also matches `scan_datagen_map_pruning`.
- The commit body holds the printed cold and warm waves, requests and bytes of `lookup_scan`.

**Commit:** `strata: scan payload and map columns, add lookup_scan (M4.3)`
