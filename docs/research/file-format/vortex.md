# Vortex: technical review as a general-purpose columnar format

This review judges Vortex as a general-purpose columnar file format. It uses five generic workload classes:
1. filtered scans over light columns;
2. point lookups by a sorted key;
3. large, repetitive strings, and substring search over them;
4. write throughput and file size;
5. local disk versus object store.

LLM-trace data (large JSON prompts and completions that repeat across neighbouring rows, string-to-string attribute maps, 16-byte binary IDs) is used only as an example dataset.

Source: `/home/sebastien/github/vortex`, HEAD `d7aa24e8f` (2026-11-13, "Make successful Arrow plugin imports final (#11464)"), 50 commits after tag `0.93.0` (2026-11-06). Releases are frequent: 0.90.0 came out on 2026-10-23, 0.91.0 on 11-04, and 0.92.0 and 0.93.0 both on 11-06. The workspace `Cargo.toml` says `version = "0.1.0"` (Cargo.toml:97); the real version is set at release time. On crates.io, `vortex`, `vortex-file` and `vortex-datafusion` are at **0.93.0**. The pins match ours: arrow 59.2 (Cargo.toml:106), DataFusion 55.0.0 (Cargo.toml:141), object_store 0.13.2. MSRV is 1.95.

**Stability.** The *file format* has been declared stable since crate version 0.36.0 and edition `core2025.05.0` (docs/specs/file-format.md:3-8; README.md:42-46). Compatibility is managed per **edition**, a named set of permitted wire IDs (docs/specs/versioning.md). Frozen editions run up to `core2026.08.3` (min 0.85.0). That edition adds `vortex.map` (core2026.08.2), `vortex.onpair` (core2026.08.1) and `vortex.zoned` with min/max/null_count aggregates (core2026.08.0) (docs/specs/versioning/editions.md). These components still sit only in the never-frozen `UNSTABLE` edition (vortex-edition/src/declarations/unstable/mod.rs:34-44):
- the List layout;
- FastLanes delta;
- `bitpacked.v2` and `for.v2`;
- the SBBF bloom-filter zone aggregate.

The *Rust API* is explicitly unstable (semver 0.x, "Library APIs may change from version to version"). The container version tag is `VERSION: u16 = 1` (vortex-file/src/lib.rs). The arrow 59.2, DataFusion 55 and object_store 0.13 pins are the current upstream releases, so Vortex slots into a current DataFusion stack without forcing different versions.

---

## 1. File layout, footer and open path

**Byte layout** (vortex-file/src/lib.rs:55-83; docs/specs/file-format.md):

`'VTXF' | segments… | [user metadata segs] | DType fb | Layout fb | Statistics fb | Footer fb | Postscript (≤65527 B) | u16 version | u16 postscript_len | 'VTXF'`

The constants are `MAX_POSTSCRIPT_SIZE = u16::MAX-8` and `EOF_SIZE = 8` (lib.rs:162-170).

- **Postscript** (footer.fbs:25-53) holds a `PostscriptSegment {offset u64, length u32, alignment_exponent u8, compression, encryption}` for each of `dtype`, `layout`, `statistics` and `footer`, plus up to 16 keyed user-metadata segments. Keeping it separate guarantees the reader has the postscript after its first tail read.
- **Footer** (footer.fbs:118-130) holds dictionary-encoded `array_specs` (encoding string IDs), `layout_specs`, and `segment_specs`. Each segment spec is a fixed 16-byte struct `{offset u64, length u32, align u8, _compression u8, _encryption u16}` (footer.fbs:148-160).
  - The compression and encryption fields are **reserved and unused** ("not used in the current version", footer.fbs:155-160), and `EncryptionSpec` is an empty table (footer.fbs:174).
  - So there is no segment-level general-purpose compression. All compression comes from the array encodings.
- **Layout tree** (vortex-layout/flatbuffers/vortex-layout/layout.fbs:18-30) is a recursive `Layout {encoding u16, row_count u64, metadata [ubyte], children [Layout], segments [uint32]}`. The whole tree for every column and every chunk is **one flatbuffer**, so its size grows with columns × chunks.
- **File statistics** (footer.fbs FileStatistics) store per-leaf aggregate partials (min/max/null_count/sum…) for whole-file pruning.

**Open path** (vortex-file/src/open.rs:360-415):
1. A `size()` call (HEAD on S3), skipped if `with_file_size` is given.
2. One tail read of `max(initial_read_size, 64 KiB)` (open.rs:42, 366-379).
3. `FooterDeserializer` returns `NeedMoreData{offset,len}` when the dtype, layout or footer segments extend beyond that read (open.rs:385-398). In that case a second, prefix read is needed.
4. Any data segment that happened to fall inside the tail read is cached (`collect_initial_segments`, open.rs:418-456).

A footer can be passed in pre-parsed (`with_footer`, open.rs:182) for zero-I/O opens. The DataFusion integration does this through DataFusion's `FileMetadataCache` (vortex-datafusion/src/persistent/cache.rs; opener.rs:291-316).

**DType model** (vortex-array/src/dtype/mod.rs:65-140) has:
- Null, Bool, Primitive, Decimal, Utf8, Binary;
- List, FixedSizeList;
- **Map** (key non-null, Arrow `keys_sorted`);
- Struct, Union, **Variant**;
- Extension (timestamps, UUID, …).

The root does not have to be a struct.

**Arrow type coverage (confirmed in source).** The Arrow→Vortex dtype mapping is `from_arrow_data_type` (vortex-arrow/src/dtype.rs:89-174). Any type not matched falls into `_ => vortex_bail!("Arrow data type not supported")` (dtype.rs:172).

Rejected on import:
- **`FixedSizeBinary(n)`, any `n`.** Confirmed by the regression test `fixed_size_binary(DataType::FixedSizeBinary(3))` (dtype.rs:620-630, issue #8346). The one exception is `FixedSizeBinary(16)` carrying the canonical `arrow.uuid` extension metadata, which the UUID plugin maps to the `vortex.uuid` extension dtype (encodings/uuid/src/arrow.rs:50-78; that dtype is in edition core2026.08.3). Fixed-width binary keys such as 16- or 8-byte IDs must otherwise become `Binary` (variable-length: offsets, no element size, so not capped by the 1 MiB block target), or `u128`/`u64`-style primitives.
- **`Duration(_)` and `Interval(_)`.** Same test (dtype.rs:624-625); the array path also bails, "Arrow {} arrays are not supported" (vortex-arrow/src/convert.rs:339-340).
- **`Union` (sparse and dense).** Not matched, so rejected on import, even though Vortex has a `DType::Union`. Export of a Vortex `Union` also bails: `"TODO(connor)[Union]: unimplemented"` (dtype.rs:278).

Accepted but lossy:
- `Dictionary(_, V)` and `RunEndEncoded(_, V)` are flattened to their value type (dtype.rs:164-169). The encoding is re-chosen by the compressor, and the Arrow dictionary or REE type is not returned on read.
- `LargeUtf8`/`Utf8View` collapse to `Utf8`, `LargeBinary`/`BinaryView` to `Binary`, and `LargeList`/`ListView` to `List` (dtype.rs:109-131).
- On export, Utf8 comes back as **Utf8View**, Binary as **BinaryView**, and List as 32-bit `List` (dtype.rs:239-247).
- A `Map` must have a non-nullable entries struct with exactly two fields and a non-nullable key (dtype.rs:139-160); malformed maps are rejected.

Supported: Null, Boolean, all integers and floats including `Float16`, `Decimal32/64/128/256`, `Date32/64`, `Time32/64`, `Timestamp` (with time zone), `Struct`, `FixedSizeList`, `Map`, and `arrow.parquet.variant` → `Variant` (dtype.rs:176-186).

## 2. Array and encoding model

An array is a tree: encoding vtable, dtype, length, buffers, children (called "slots"), and lazily computed stats (docs/concepts/arrays.md). Each logical type has one **canonical** form:
- Primitive/Bool for scalars;
- `VarBinViewArray` for Utf8/Binary;
- `ListViewArray` for List;
- Struct and Extension otherwise.

Everything else is an encoding that can be "executed" toward canonical form. Execution first looks for an encoding-specific kernel and canonicalizes only as a fallback (arrays.md "Execution").

Compute-on-compressed kernels exist per encoding (the `compute/` directories):

| Encoding | Kernels present |
|---|---|
| bitpacked (fastlanes) | `compare`, `compare_fused`, `between`, `filter`, `take`, `stream_predicate` |
| FoR | `compare`, `is_sorted` |
| ALP | `compare`, `between`, `filter`, `take` |
| Dict | `compare`, `like` (on the values, then mapped through the codes), `filter`, `min_max` |
| RunEnd | `compare`, `filter`, `take`, `sum` |
| Sequence | `compare`, `list_contains`, `take` |
| Sparse | `compare`, `like`, `between` |
| FSST | `compare`, `like` (DFA on codes), `filter`, `byte_length` |
| OnPair | `compare` (equality only), `filter`, `take`, `byte_length` |
| **Zstd** | only `cast`; everything else decompresses |

**Random access (`scalar_at`) cost:**
- **Bitpacked**: O(1). `unpack_single` and the blocked variant (encodings/fastlanes/src/bitpacking/vtable/operations.rs:17-35), plus a patch lookup.
- **FoR, ALP, Dict**: O(1) on top of the child.
- **Delta**: decodes the whole 1024-value FastLanes chunk ("a read decodes the whole 1024-value chunk", encodings/fastlanes/src/delta/probe.rs:1-7).
- **RunEnd**: binary search over the run ends (vortex-array/src/arrays/runend/ops.rs:30-50).
- **Pco**: decodes the containing page and keeps it in the probe state (encodings/pco/src/probe.rs header).
- **FSST and OnPair**: decompress only the one row (encodings/fsst/src/ops.rs:45-60). This is the point of FSST/OnPair: per-row random access with no block decode.
- **Zstd**: `scalar_at` slices, then decompresses **every frame overlapping the row** (encodings/zstd/src/array/mod.rs:1530-1543, 1290-1345). The string scheme writes 8192 values per frame (vortex-btrblocks/src/schemes/string/zstd.rs: `from_var_bin_view_without_dict(&compacted, 3, 8192, …)`), and a chunk is 8192 rows, so one row means decompressing the whole chunk.

Statistics travel with arrays: null_count, min/max, is_sorted, is_constant, run_count, uncompressed size (arrays.md "Statistics"). The compressor uses them to choose schemes cheaply.

## 3. Compression: BtrBlocks-style cascade with sampling

`CascadingCompressor` (vortex-compressor):
- **Pass 1** gathers immediate verdicts from each scheme (`Skip`, `AlwaysUse`, or a ratio computed from stats).
- **Pass 2** runs deferred estimates: `Sample` estimates first, then `Callback` estimates, which receive the current best score so they can bail early (vortex-compressor/src/compressor/select.rs:50-120).

Sampling (compressor/sample.rs:27-85) takes `SAMPLE_COUNT=16` random slices of `SAMPLE_SIZE=64` rows: about 1% of the rows, with a minimum of 1024 values and a fixed seed. **On an 8192-row chunk that minimum is 12.5% of the data**, and *every* candidate scheme compresses that sample, including its child cascades. The winner then compresses the full chunk. Cascades go at most `MAX_CASCADE = 3` deep (vortex-compressor/src/scheme/ctx.rs:16).

**Default scheme set** (vortex-btrblocks/src/builder.rs). `CompressionMode::Default` **excludes Zstd and Pco**. `with_compact()` re-enables Zstd for strings and binary and Pco for numerics. The cascade covers:
- Integers: FoR, BitPacking, Delta (unstable), RLE, RunEnd, Sequence, Sparse, ZigZag, Dict.
- Floats: ALP, ALP-RD, RLE, Sparse.
- Temporal: DateTimeParts (days, seconds and subseconds split, schemes/temporal.rs).
- Strings: Dict, FSST, OnPair (12-bit dictionary), and null-dominated Sparse.

Before any of this, a `DictStrategy` probes the first 8192 rows of each column for a column-level dictionary layout (max 1 MiB or 65 535 entries; vortex-layout/src/layouts/dict/writer.rs:68-115, file/src/strategy.rs:257-266).

The string encodings:
- **FSST** (encodings/fsst): 255 symbols of at most 8 bytes, trained per chunk. Its children (lengths and code offsets) are compressed further by the cascade (schemes/string/fsst.rs:50-105).
- **OnPair** (encodings/onpair, crate `onpair` 0.3.0) learns frequent adjacent substrings with `max_dict_bits = 12` (4096 tokens) and **`MAX_TOKEN_SIZE = 16` bytes** (onpair-0.3.0/src/core/types.rs:18, encoding/config.rs:86-94). It "usually wins on ratio" against FSST (schemes/string/fsst.rs doc comment).
- **Zstd** (Compact mode only) uses level 3, no dictionary, and 8192 values per frame.

**Write-time CPU** has four costs:
- Canonicalization at repartition (`canonicalize: true`, strategy.rs:104).
- Zone stats per 8192 rows (spawned on the CPU pool, zoned/writer.rs:172, 260).
- One trial compression per candidate scheme on the sample.
- The final compression.

`CompressingStrategy` runs `get_available_parallelism()` chunks **per column** concurrently (vortex-layout/src/layouts/compressed.rs:63-124). On a wide table (tens of columns) whose large-string column has 8192-row chunks of hundreds of MB each, peak writer memory can become very large (see §6). The README claims "5x faster writes" than Parquet, but that comes from analytic benchmarks with mostly narrow values.

## 4. Pruning, filter pushdown and late materialization

**Default write layout** (vortex-file/src/strategy.rs:218-327; docs/concepts/file-format.md):

`Table/Struct (split columns) → Repartition (8192-row multiples) → Zoned (stats per 8192 rows) → Dict-or-fallback → Repartition (coalesce to ≥1 MiB uncompressed, multiples of 8192 rows) → Compress → Buffered (2 MiB) → Chunked → Flat (one segment per chunk)`

There are **no row groups**. Every column is chunked independently. Segments are emitted in sequence-ID order, so the chunks of all columns for a given row range land near each other (vortex-file/src/segments/writer.rs:33-90).

**Zone maps** (vortex-layout/src/layouts/zoned/writer.rs:59-80, 316-348):
- 8192-row zones.
- `bounded_min`/`bounded_max` truncated to **64 bytes** for strings and binary (zoned/schema.rs:235-238); exact `min`/`max` for other types.
- `nan_count` and `null_count`. Sums are deliberately not stored.
- Bloom filters exist as a zone aggregate but are only in `UNSTABLE`.

The zones table is itself a compressed flat array. It is evaluated once per predicate, and the resulting zone mask is expanded to a row mask (zoned/reader.rs:150-200).

**Scan** (vortex-layout/src/scan/; the `LayoutReader` trait in vortex-layout/src/reader.rs:246-313) runs three phases per split:
1. `pruning_evaluation`: zone maps produce a mask of rows that cannot match.
2. `filter_evaluation`: reads only the filter columns and refines the mask.
3. `projection_evaluation`: reads the output columns with the final mask.

The filter is split into conjuncts. Each conjunct keeps a selectivity histogram and the conjuncts are reordered dynamically. Unselective *dynamic* conjuncts (for example DataFusion TopK filters) can be skipped (scan/filter.rs header). Columns used only by the filter are dropped before output (docs/concepts/scanning.md).

At the flat level:
- If the mask density is below `EXPR_EVAL_THRESHOLD = 0.2`, the reader filters first and then evaluates; otherwise it evaluates over the whole chunk and ANDs the masks (layouts/flat/reader.rs:38, 127-183).
- Projection awaits the mask first. **A chunk whose rows are all false is never decoded**, and its segment future is dropped before it is polled (flat/reader.rs:198-226).
- **The granularity is the whole segment.** There is a TODO to slice before evaluating (flat/reader.rs:143-145), and the comment at strategy.rs:95-100 says "Vortex never reads less than one segment".

**Two scan engines coexist:**
- **V1** is the `ScanBuilder` over `LayoutReader`.
- **V2** (`scan/v2`, `plan/`) lowers the layout once to a physical plan. It cuts work into 64K–1M-row chunks independent of the storage chunks (scan/v2/mod.rs header). It is off by default in DataFusion (`scan_v2: bool, default = false`, vortex-datafusion/src/persistent/format.rs:204-210).

The **indexed layout** (pluggable secondary indexes) is labelled "a prototype" (vortex-layout/src/layouts/indexed/mod.rs:3-4). The only index implementations are in tests and an example (`examples/reverse_index`).

**DataFusion integration** (vortex-datafusion):
- `VortexFormatFactory` and `VortexFormat` implement `FileFormat` for ListingTable and `CREATE EXTERNAL TABLE`.
- `v2::VortexTable` is a `TableProvider` over a Vortex `DataSource`.
- The opener handles the rest (persistent/opener.rs):
  - file-level pruning through DataFusion's `FilePruner`;
  - a footer cache;
  - a `PartitionedFile.range` translated to a row range proportional to the bytes (`partition_row_range`, opener.rs:468-514);
  - a native limit when no residual filter remains;
  - dynamic filters.
- Predicate pushdown is on by default and *projection* pushdown is off (format.rs:216-231).

The expression converter (convert/exprs/mod.rs) pushes down:
- binary comparisons and arithmetic, IN lists, casts;
- `LIKE` (case-insensitive is converted, but FSST falls back), `contains`/`starts_with`/`ends_with` (rewritten as LIKE), `strpos`, `lower`, `octet_length`;
- `get_field` **on Struct only**: a Map source returns `Unsupported` (mod.rs:688-722);
- `array_length`, `array_sum`, `array_transform`.

Anything else stays in DataFusion as a residual filter. Map key lookups such as `attributes['k']` are not pushed down. Vortex has no map-get scalar function at all (vortex-array/src/scalar_fn/fns).

## 5. I/O

`FileSegmentSource` (vortex-file/src/segments/source.rs:62-90, 300-330) gives each segment future four states: *registered → requested (polled) → in-flight → resolved*. Registered but unpolled requests are **eligible for coalescing** with polled ones. Requests are served in registration order, and dropping a future cancels it.

A background `ReadDriver` keeps up to `concurrency` ranges in flight through `VortexReadAt::read_ranges` (source.rs:190-265). Coalescing (vortex-file/src/read/driver.rs) merges requests within a distance window, up to a maximum size:

| Source | Coalesce settings | Concurrency |
|---|---|---|
| Local file (`FileReadAt`, pread) | 1 MiB distance, 4 MiB max (`CoalesceConfig::file()`) | 32 |
| Object store | 1 MiB distance, 16 MiB max | 192 |
| In-memory | 8 KiB | — |

Sources: vortex-io/src/read_at.rs:59-77, std_file/read_at.rs:61, 118-123, object_store/read_at.rs:36, 75.

The object-store path issues one ranged GET per coalesced range. It deliberately avoids `get_ranges`, so each result streams out as soon as it is ready (object_store/read_at.rs:206-245).

A driver FIXME says the coalescing should be split into a `coalesce_distance` and a `max_read_size` (read/driver.rs:44-45). Because registered requests can be merged in, segments that the zone map pruned may still be fetched when they are within 1 MiB of a needed one. That is deliberate read amplification, good for S3 and wasteful for point lookups.

There is no prefetch beyond the scan's split concurrency. Optional caching:
- a `MokaSegmentCache` (vortex-layout/src/segments/cache.rs);
- `with_layout_reader_cache` to reuse the reader tree and dictionary values across scans (open.rs:121-130).

## 6. Fit with the generic workload classes

### Filtered scans over light columns
This is Vortex's strength.
- Nullable integers and floats get FoR+bitpacking or ALP, and comparisons and `between` run on the compressed form.
- Low-cardinality strings become dict or RLE, with compare and LIKE evaluated on the dictionary values.
- Columns referenced only by the filter are read, evaluated, then dropped.
- Conjuncts are reordered adaptively.

Zone pruning (8192-row zones) works only for columns that are sorted or clustered in the file. Pruning a column that is not clustered (for example a timestamp when rows are sorted by another key) relies on file-level statistics (`VortexFile::can_prune`, file.rs:284) or on the caller partitioning files.

### Point lookups by a sorted key
1. The zone map (bounded min/max of at most 64 B, 8192-row zones) narrows the search to one or two zones.
2. `filter_evaluation` decodes the key column's chunk.
3. Projection fetches **one whole segment per projected column**.
4. Nothing binary-searches a sorted column or keeps a key→row index. The SBBF bloom filter is in `UNSTABLE` and the indexed layout is a prototype.
5. `ScanBuilder::with_row_indices` gives `take` by row number, which the random-access benchmark uses.

For narrow columns the cost is about one coalesced I/O per column. For wide-value columns the cost is set by segment size (next section).

### Large, repetitive strings (e.g. 1–100 KB JSON prompts sharing long prefixes with neighbouring rows)
- **Default mode (FSST, OnPair, Dict).** None of these exploit long-range redundancy.
  - FSST symbols are at most 8 bytes, so a 30 KB prefix repeated across rows still costs about 1 code byte per 3–8 input bytes *in every row*.
  - OnPair tokens are at most 16 bytes, with 4096 of them, trained per chunk.
  - At best expect about 2–4× on such data, where zstd or LZ with a long window would get an order of magnitude more on prefix repetition.
  - Whole-string dictionary encoding helps only on exact duplicates. It is capped at 1 MiB and 65 535 entries and decided from the first 8192 rows (dict/writer.rs:92-120).
- **Compact mode (zstd).** Level 3, no dictionary, 8192 values per frame. The zstd window does capture repetition between neighbouring rows inside a frame (so sorting related rows together helps). **But** a single-row read decompresses the whole frame (§2).
- **Chunk sizing is the critical defect.** `effective_block_len` caps rows per block by bytes only for fixed-width dtypes. For Utf8 and Binary, "block_len_multiple is unchanged" (vortex-layout/src/layouts/repartition.rs:56-76, 326). Every flat segment of a variable-width column is therefore **8192 rows, or a multiple of it**: at 20 KB per value that is about 160 MB uncompressed per segment.
  - Segment length is a `u32`, and the writer errors above 4 GiB (segments/writer.rs:58-60).
  - A point lookup fetches the *entire* compressed segment (tens of MB) for each projected large column, even though FSST and OnPair could decode one row cheaply.
  - Workarounds: a per-field writer (`WriteStrategyBuilder::with_field_writer`) with a small row block, or a global `with_row_block_size(256..1024)`, which also shrinks zones for every column.
- **Writer memory.** Each column runs `available_parallelism` chunks at once (compressed.rs:73-124). Wide tables with 100+ MB string chunks can reach several GB of peak RSS unless the concurrency is lowered by hand.

### Substring and full-text search
- FSST has a real DFA over compressed codes for `prefix%` and `%needle%`: a KMP failure function, then a symbol-level transition table (encodings/fsst/src/dfa/mod.rs:1-60).
  - Needle at most 254 bytes (dfa/flat_contains.rs:95).
  - Case-sensitive only: `ILIKE` falls back (compute/like.rs:32-34).
  - Suffix matching is still TODO (dfa/mod.rs:16-21).
- OnPair supports only equality.
- Zstd decompresses everything.

There is no inverted, n-gram or token index, so `%needle%` is a full scan of the column. The scan runs in the compressed domain only when FSST won the scheme selection.

### Nested and semi-structured data (Map, List)
- `vortex.map` is a ListView of `{key,value}` structs, compressed by the cascade (vortex-array/src/arrays/map/array.rs; vortex-compressor/src/compressor/cascade.rs:190). It is a **leaf**: no per-key columns, no key-level zone maps, and no map-get scalar function, so `m['k'] = v` cannot be pushed down. The DataFusion converter rejects `get_field` on non-struct sources (convert/exprs/mod.rs:709-711).
- Lists use the flat leaf path unless the `UNSTABLE` List layout is enabled (strategy.rs:146-149, 301-324; table.rs:300-308).
- `Variant`, with shredding, exists (DType::Variant, `vortex.parquet.variant`) and is the intended home for semi-structured attributes, but it is new (core2026.08.3).

### Write throughput and file size
- Per-chunk costs: canonicalization, zone stats, a trial compression of every candidate scheme on a sample (at least 12.5% of an 8192-row chunk), then the final compression with cascades up to depth 3.
- Narrow analytic data compresses about as well as Parquet+zstd, by the project's own claim.
- Large-text columns are 2–5× bigger than zstd in default mode (no long-range matching).

### Local disk vs object store
- Both use the same segment model. Local disk uses pread, 32 concurrent requests and a 4 MiB coalescing cap. S3 and object stores use 192 concurrent ranged GETs and a 16 MiB cap.
- Opening costs a HEAD, which `with_file_size` avoids, plus a 64 KiB tail GET and sometimes a second footer GET.
- The footer can be cached and injected, giving zero-I/O opens.

## 7. Weaknesses, maturity and unfinished areas

- **API churn.** About weekly 0.x releases (four in two weeks). Two scan engines (V1/V2) and two statistics formats (legacy `field_stats` and aggregate partials, footer.fbs FileStatistics) coexist. Many components are only in `UNSTABLE`: List layout, delta, v2 bitpacking/FoR, bloom filters. Docs say the Scan API "is on the roadmap and under active development" (docs/concepts/scanning.md:3-6).
- **Format behaviour switched by environment variable.** `FLAT_LAYOUT_INLINE_ARRAY_NODE=1` changes whether the array encoding tree is inlined in the layout metadata (vortex-layout/src/layouts/flat/mod.rs:36-41).
- **Reserved but unimplemented fields.** Segment-level compression and encryption are reserved, and the "fine-grained encryption" in the spec does not exist.
- **The whole layout tree is one flatbuffer.** With a fixed 64 KiB tail read, a wide file with many chunks needs a second footer round trip.
- **Whole-segment reads.** No sub-segment or page reads. Filter evaluation decodes the whole chunk before masking (TODO at flat/reader.rs:143).
- **Compression scope.** Defaults are tuned for analytics on narrow values. There is no long-range string compression in the default mode, and the zstd scheme has no dictionary or long-distance matching.
- **FixedSizeBinary** is unsupported unless it carries the UUID extension.
- **Arrow round trip.** Strings and binary always come back as View types, and Arrow dictionary types are lost.
- **No real secondary indexes** (indexed layout prototype; Elias-Fano and Unary are "not in a file edition", encodings/elias-fano/src/lib.rs:1-12).
- **`unsafe`** is used widely (about 1,100 occurrences in vortex-array, about 210 in encodings), even for `NonZeroUsize::new_unchecked(8192)` (zoned/writer.rs:73). That is an audit surface for projects that forbid `unsafe`.
- **Benchmark claims** (README.md:22-26): "100x faster random access, 10-20x faster scans, 5x faster writes, similar compression ratios". The random-access benchmark is fair to Parquet: `PageIndexPolicy::Required` plus `RowSelection` (vortex-bench/src/random_access/take.rs:216, 297, 362-376). But the datasets are TPC-H, ClickBench and similar narrow-row data. The string-heavy FineWeb benchmark measures FSST and dict, with no 100 KB values. No benchmark resembles 100 KB repetitive JSON.

## 8. Dependencies and minimal code

Versions from `cargo search` and `cargo info` (network OK):

```toml
vortex = { version = "0.93.0", default-features = false, features = ["files", "zstd", "tokio", "object_store"] }
vortex-datafusion = "0.93.0"   # requires datafusion 55, arrow 59.2, object_store 0.13.2 (same family as ours)
```

`vortex`'s default features are `files, parquet-variant, wasm-bindgen, zstd`. Licence is Apache-2.0 and MSRV is 1.95 (ours is 1.99.1). The API names below were checked in the source at HEAD (0.93.0 + 50 commits).

```rust
use std::sync::Arc;
use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use futures::{Stream, StreamExt, TryStreamExt};
use vortex::VortexSessionDefault;                       // VortexSession::default()
use vortex::session::VortexSession;
use vortex::array::stream::{ArrayStreamAdapter, ArrayStreamExt};
use vortex::array::expr::{and, col, eq, gt_eq, lit, root, select};
use vortex::arrow::ArrowSessionExt;                     // session.arrow()
use vortex::file::{OpenOptionsSessionExt, WriteOptionsSessionExt, WriteStrategyBuilder};
use vortex::compressor::BtrBlocksCompressorBuilder;
use vortex::error::VortexResult;

async fn write(session: &VortexSession, schema: SchemaRef,
               batches: impl Stream<Item = RecordBatch> + Send + 'static) -> VortexResult<()> {
    let dtype = session.arrow().from_arrow_schema(&schema)?;          // vortex-arrow/src/session.rs:738
    let s2 = session.clone();
    let sc = schema.clone();
    let stream = batches.map(move |rb| s2.arrow().from_arrow_record_batch(rb, &sc)); // session.rs:752
    let strategy = WriteStrategyBuilder::from_session(session)       // vortex-file/src/strategy.rs:136
        .with_row_block_size(1024)                                    // smaller zones/segments
        .with_btrblocks_builder(BtrBlocksCompressorBuilder::from_session(session).with_compact())
        .build();
    let mut f = tokio::fs::File::create("/tmp/spans.vortex").await?;  // VortexWrite: vortex-io/src/tokio.rs:11
    session.write_options()
        .with_strategy(strategy)                                      // writer.rs:131
        .write(&mut f, ArrayStreamAdapter::new(dtype, stream))        // writer.rs:262
        .await?;
    Ok(())
}

async fn scan(session: &VortexSession) -> VortexResult<()> {
    let file = session.open_options().open_path("/tmp/spans.vortex").await?; // open.rs:231
    let filter = and(eq(col("status"), lit("error")), gt_eq(col("cost_total"), lit(0.01f64)))
        .bind(file.dtype())?.optimize_recursive()?;
    let projection = select(["span_id", "name", "cost_total"], root()).bind(file.dtype())?;
    let builder = file.scan()?.with_filter(filter).with_projection(projection); // file.rs:254
    let out_schema = Arc::new(session.arrow().to_arrow_schema(&builder.dtype()?)?);
    let mut batches = builder.into_record_batch_stream(out_schema)?;  // vortex-layout/src/scan/arrow.rs:48
    while let Some(rb) = batches.try_next().await.map_err(|e| vortex::error::vortex_err!("{e}"))? {
        let _ = rb.num_rows();
    }
    Ok(())
}
```

`ScanBuilder::with_filter` and `with_projection` take a `BoundExpression`. `with_row_indices`, `with_row_range`, `with_limit` and `with_concurrency` also exist (scan_builder.rs:139-281). Before shipping, check the exact `lit()` type for a Utf8 dict column and the shape of the `vortex_err!` macro; they are not compiled here.

DataFusion path (vortex-datafusion/src/lib.rs:20-50; persistent/format.rs:327-460):

```rust
use datafusion::datasource::listing::{ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl};
use vortex_datafusion::{VortexFormat, VortexTableOptions};
let opts = VortexTableOptions { predicate_pushdown: true, projection_pushdown: true, ..Default::default() };
let fmt = Arc::new(VortexFormat::new_with_options(VortexSession::default(), opts));
let cfg = ListingTableConfig::new(ListingTableUrl::parse("file:///data/spans/")?)
    .with_listing_options(ListingOptions::new(fmt).with_file_extension(".vortex"))
    .with_schema(arrow_schema);
ctx.register_table("spans", Arc::new(ListingTable::try_new(cfg)?))?;
// or: register VortexFormatFactory::new() and use `CREATE EXTERNAL TABLE … STORED AS vortex`
```

---

## Ideas for a faster format: adopt or avoid (ranked)

1. **Adopt:** a cascading, sampling-based per-chunk encoding choice (FoR/bitpack/ALP/RLE/dict/sequence) for light columns, with compare and between kernels that run on the compressed form.
2. **Adopt:** a postscript of ≤64 KiB that lets the reader find the footer in one tail read, plus an injectable pre-parsed footer for cache hits with zero I/O.
3. **Avoid:** fixed row-count chunking for variable-width columns. Size string and payload segments by bytes (e.g. 256 KB–1 MB compressed), independent of zone size.
4. **Adopt:** long-range compression for large string values: zstd or LZ4 with a large window or a per-trace or per-file trained dictionary, in small frames (≈64–256 KB) with a row→frame index.
5. **Avoid:** making a single-row read decompress or fetch a whole chunk. Keep an offset index so a point lookup reads one small frame per payload column.
6. **Adopt:** FSST-style compressed-domain LIKE (a KMP-to-symbol DFA) for short or medium strings, and add case-insensitive and suffix support from day one.
7. **Adopt:** a real per-file key→row-range index for the sort key (a tiny sparse index or Elias-Fano suffices) instead of relying on 8192-row zone maps or bloom filters.
8. **Adopt:** three-phase scans: prune with zones, evaluate the filter on filter columns only, then project with the mask and skip the decode of all-false chunks; also adaptive reordering of conjuncts by measured selectivity.
9. **Adopt:** segment futures that register lazily and coalesce on poll, with per-backend coalesce and concurrency settings (local: 1 MiB gap / 4 MiB max / 32 in flight; S3: 1 MiB / 16 MiB / 192 in flight).
10. **Avoid:** coalescing pruned (unpolled) segments into reads by default for point-lookup paths; use a separate gap policy per query type.
11. **Adopt:** emitting segments in row-block order across columns (sequence IDs), so that one row range's columns are physically close.
12. **Avoid:** one monolithic layout flatbuffer for all columns × chunks; split per-column metadata so wide files open with bounded footer reads.
13. **Adopt:** Map stored as shredded keys (top-N keys as real columns with stats) plus a residual map; Vortex's leaf `Map` with no key pushdown is the anti-pattern for `attributes['k']` filters.
14. **Avoid:** unbounded per-column compression parallelism in the writer; bound in-flight bytes globally, not by chunk count per column.
15. **Adopt:** an n-gram, token or bitmap index over payload text for substring search, since no columnar encoding makes `%needle%` over 100 KB values fast.
16. **Avoid:** reserved-but-unimplemented format fields and env-var-dependent wire output; version the wire format explicitly (Vortex's edition and wire-ID scheme is a good model).
17. **Avoid:** lossy Arrow type mapping (FixedSizeBinary rejected, dictionary types dropped); support FixedSizeBinary, Duration, Interval and Union natively, since fixed-width binary IDs are common hot keys.
18. **Adopt:** truncated (64 B) string min/max in zone maps plus null counts, and keep sums out of zone maps.
