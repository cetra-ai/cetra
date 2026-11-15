# Apache Parquet and arrow-rs `parquet` 59.3.2: a review to inform a faster columnar format

**Sources read:**
- The format spec at `/home/sebastien/github/parquet-format` (HEAD `ec90dd4`, format 2.14+ with FINITE_VECTOR). Cited as `fmt/…`.
- arrow-rs `parquet` 59.3.2 at `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/parquet-59.3.2/src`. Cited as `src/…`.
- DataFusion 55.2 for engine defaults, and `object_store` 0.14.2.

**How it is judged.** Five generic workload classes:
- **(A)** filtered scans over light columns;
- **(B)** point lookups by a sorted key;
- **(C)** large repetitive strings and substring search;
- **(D)** write throughput and file size;
- **(E)** local disk versus object store.

The running example is LLM-trace-like data. It has:
- 16-byte IDs;
- timestamps;
- low-cardinality strings;
- nullable integer and float metrics;
- `Map<string,string>` attributes and string lists;
- very large JSON prompt/completion strings (1–100 KB per row) that repeat heavily across neighbouring rows.

**Notation.** R = row groups per file, C = leaf columns, P = pages per column chunk. The numbers below are worked from the code and the defaults. They are estimates, not benchmarks.

---

## 1. Physical layout and metadata

**Layout.** A file looks like this (`fmt/README.md:92-120`):

`PAR1` · R × (C column chunks, each a run of pages) · `FileMetaData` (Thrift compact) · 4-byte length · `PAR1`

- **Pages.** Every page starts with a Thrift `PageHeader` (`fmt/src/main/thrift/parquet.thrift:844-878`).
- **Dictionary page.** If a chunk has one, it comes first, and there is at most one (`fmt/README.md:207-218`).
- **Page index.** The ColumnIndex and OffsetIndex sit between the data and the footer.
- **Bloom filters.** arrow-rs writes them **after each row group** by default (`BloomFilterPosition::AfterRowGroup`, `src/file/properties.rs:50,173-184`).

**What the footer holds.** `FileMetaData` (thrift:1422) contains:
- a flattened depth-first `list<SchemaElement>` (thrift:538);
- `row_groups`;
- `key_value_metadata`;
- `column_orders`.

Each `RowGroup` (thrift:1063) holds one `ColumnChunk` per leaf (thrift:1005). Each `ColumnChunk` embeds a full `ColumnMetaData` (thrift:922-987), which contains:
- the type;
- the list of encodings;
- **`path_in_schema` as a list of strings, repeated in every row group**;
- the codec;
- value counts and sizes;
- the page offsets;
- `Statistics` (binary min/max, null and distinct counts; thrift:267);
- `encoding_stats`;
- the Bloom filter offset and length;
- `SizeStatistics` (level histograms and unencoded byte-array size; thrift:202).

`ColumnChunk` also stores the offset and length of its own page-index entries (thrift:1042-1050).

The footer is therefore **O(R × C)** and has no random access: to reach the metadata of column *c* in row group *r*, a reader must parse every struct that comes before it. A footer extension example adds a metadata checksum; the current footer has none (§5.9).

**What a reader must do to open and prune a file:**
1. **Fetch the footer.** It reads the last 8 bytes, then the footer. On an object store that is two GETs unless a prefetch hint is given (`src/file/metadata/reader.rs:150-158`). DataFusion's default hint is 512 KiB (`datafusion-common-55.2.0/src/config.rs:1227`).
2. **Decode the whole footer.** `parquet_metadata_from_bytes` (`src/file/metadata/thrift/mod.rs:743-860`) walks every row group. `read_row_group` (`:628-700`) walks every column and checks that the count matches the schema's leaf count (`:665-671`).
   - Options can skip *materialising* column statistics, encoding stats and size stats for some columns (`src/file/metadata/options.rs:48-112`, applied at `thrift/mod.rs:423-428`). The skipped bytes are still walked by `prot.skip()`, one varint at a time.
   - The decoder ignores `path_in_schema` (`thrift/mod.rs:468`). The writer still emits it by default (`DEFAULT_WRITE_PATH_IN_SCHEMA = true`, `src/file/properties.rs:74`; `thrift/mod.rs:1341`).
3. **Fetch the page index, if pruning by page.** `range_for_page_index` (`src/file/metadata/push_decoder.rs:485-499`) takes the **union** of the index ranges of *all columns of all row groups*. `parse_column_index` and `parse_offset_index` (`src/file/metadata/parser.rs:244-320`) then decode every one of them, projected or not.
   - The OffsetIndex has a hand-written fast path (`src/file/page_index/index_reader.rs:45-56`).
   - The ColumnIndex is decoded generically into typed min/max vectors (`:72-110`).
4. **Fetch Bloom filters**, with one ranged read per (row group, column) probed (§3).

**Cost formulas:**

| Item | Size |
|---|---|
| Footer | about R × C × s bytes, with s ≈ 150–300 B per chunk (page-level stats; string min/max truncated to 64 B by default, `src/file/properties.rs:54,66`) |
| Page index | about R × C × P × (10–15 B OffsetIndex + up to ~2×64 B ColumnIndex for strings) |

Example: a 60-leaf schema with 30 row groups gives 1,800 `ColumnChunk` structs and a footer of roughly 0.3–0.5 MB. That is on the order of a millisecond of CPU and thousands of small allocations per open, before any data is read. The page index is all-or-nothing across columns, so a query projecting 10 of 60 columns still decodes all 60. Engines hide this cost with a metadata cache; cold opens and wide listings still pay it.

**Where the spec is heading:**
- A Flatbuffers footer for wide tables is in Draft/PoC (`fmt/proposals/README.md:57-61`, issue #530).
- `BinaryProtocolExtensions.md` reserves Thrift field id 32767 as an escape hatch. Its example adds a FlatBuffers `FileMetaData` with CRC32 over both the footer and its length, and a final `PAR3` trailer (`fmt/BinaryProtocolExtensions.md:57-110`).
- `FileMetaData.version` still has no agreed meaning (thrift:1424-1432).

---

## 2. Encodings and compression

**Value encodings in the spec** (`fmt/Encodings.md`; enum at thrift:599-673):

| Encoding | Notes |
|---|---|
| PLAIN (Encodings.md:50) | Byte arrays are stored as a 4-byte length plus the bytes, interleaved. |
| RLE_DICTIONARY (:76) | One dictionary page per chunk plus RLE/bit-packed ids. Falls back to PLAIN when the dictionary grows too large. |
| RLE/bit-packing hybrid (:92-171) | ULEB128 run headers; bit-packed groups of 8. Used for levels, dictionary ids and booleans. |
| DELTA_BINARY_PACKED (:201-320) | Blocks of 128 values made of 32/64-value miniblocks, each with its own bit width. Zigzag ULEB first value and min-delta. |
| DELTA_LENGTH_BYTE_ARRAY (:322) | Delta-coded lengths followed by the concatenated bytes. |
| DELTA_BYTE_ARRAY (:345) | Prefix (front) coding against the **previous value only**. |
| BYTE_STREAM_SPLIT (:367) | Transposes bytes for fixed-width types. Does not shrink data; only helps the codec. |
| ALP (:397, `fmt/AlpEncoding.md`) | **Preview as of 2026-08.** Decimal-scaled floats, then frame-of-reference plus bit-packing in 1,024-value vectors, with exceptions. A 7-byte page header and a **per-vector offset array** give random access at vector granularity (AlpEncoding.md:26-110). |

**Codecs** (thrift:684-693): UNCOMPRESSED, SNAPPY, GZIP, LZO, BROTLI, LZ4 (deprecated), ZSTD, LZ4_RAW. Every page body is compressed **independently and without framing** (`fmt/Compression.md:24-38`).

**Open proposals** (`fmt/proposals/README.md:57-61`): FSST (#531) and ALP (#533).

**What arrow-rs 59.3.2 implements and uses by default:**
- **Missing encodings.** It has neither ALP nor FSST. Its `Encoding` enum stops at `BYTE_STREAM_SPLIT = 9` (`src/basic.rs:388-451`).
- **Writer defaults** (`src/file/properties.rs:30-80`):
  - `WriterVersion::PARQUET_1_0`;
  - compression **UNCOMPRESSED**;
  - dictionary on, with a 1 MiB dictionary page limit, after which it falls back;
  - 1 MiB data pages, capped at 20,000 rows;
  - page-level statistics, but no statistics in page headers;
  - up to 1,048,576 rows per row group, with **no byte limit** unless one is set;
  - Bloom filter fpp 0.05.
- **Byte-array fallback.** In v1 mode, a byte-array column that is not dictionary-encoded falls back to **PLAIN**. Only `PARQUET_2_0` switches it to DELTA_BYTE_ARRAY (`src/arrow/arrow_writer/byte_array.rs:136-140`).
- **Numbers.** Integers and floats default to PLAIN. BYTE_STREAM_SPLIT and DELTA are opt-in per column.
- **v1 pages.** They compress the repetition and definition levels *together with* the values. To learn nulls or list boundaries, a reader must decompress the whole page; the v2 header exists to avoid exactly this (thrift:759-799).

**Decode cost per value:**
- **RLE/bit-pack.** Unpacking uses generated fixed-width kernels over batches of 8/16/32/64 (`src/util/bit_pack.rs:18-60`, dispatched from `src/util/bit_util.rs:685-723`), behind a run-header state machine (`src/encodings/rle.rs:426,497`). There is no run directory, so random access means walking runs from the start of the page.
- **DELTA_BINARY_PACKED.** Decoding is a prefix sum. Even `skip()` must unpack and accumulate every skipped value to keep `last_value` (`src/encodings/decoding.rs:841-915`); only miniblocks with bit width 0 are a shortcut. Seeking to row *k* costs O(k).
- **PLAIN byte arrays.** `read` and `skip` both chase pointers (read the length, then jump) (`src/arrow/array_reader/byte_array.rs:374-442`).
  - The `StringArray` path copies every value (`:410`) and validates UTF-8.
  - The `Utf8View` path is zero-copy: views point into the decompressed page buffer (`src/arrow/array_reader/byte_view_array.rs:338-350`). DataFusion enables view types by default (`config.rs:1246`). The catch is that any surviving view keeps its whole decompressed page alive.
- **DELTA_LENGTH_BYTE_ARRAY.** Skips by summing lengths that are already decoded, without touching the data (`byte_array.rs:526-536`).
- **Decompression is page-at-a-time.** `decode_page` (`src/file/serialized_reader.rs:393-465`) does, for each page:
  1. checks the CRC, if one is present;
  2. allocates a new `Vec` of `uncompressed_page_size`;
  3. decompresses the whole page;
  4. checks the resulting size.

  Nothing is reused or decompressed partially. Within one column the pages are decompressed and decoded serially in one task. Parallelism exists only across files, row groups or byte ranges, at the engine level.

---

## 3. Pruning machinery and its granularity

**Chunk statistics** (thrift:267). Each (row group, column) has min, max and null_count. arrow-rs truncates string bounds to 64 B by default.
- They are useful only where data is sorted or clustered.
- On random 128-bit IDs sorted within a file, each row group's range covers that group's share of the key space. That is good when R is large and useless when R = 1.
- They cannot prune substring predicates on text.
- They cannot prune map-key/value predicates. In a `Map<string,string>`, every key shares one `value` leaf, so its min/max mixes all keys.

**Page index** (`fmt/PageIndex.md`; thrift:1250-1330) has two parts:
- **OffsetIndex**: `PageLocation{offset, compressed_page_size, first_row_index}` per page, plus optional unencoded sizes;
- **ColumnIndex**: `null_pages`, min/max lists, `boundary_order`, null counts and level histograms.

Its stated goals (`PageIndex.md:36-50`) are that a point lookup on the sort column reads one page per column, and that selective predicates fetch only the matching pages of other columns. Secondary indexes are an explicit **non-goal** (`:52-55`). When an OffsetIndex is present, pages must start on row boundaries (thrift:715-720, 1260-1263).

Granularity is one page, so under arrow-rs defaults one interval covers up to 20,000 rows or 1 MiB. On unsorted, high-cardinality or skewed columns (a numeric cost threshold, an `error` status in mostly-`ok` data), almost every page overlaps the predicate.

**Bloom filters** (`fmt/BloomFilter.md`):
- **Structure.** Split-block design: 256-bit blocks of 8 × 32-bit words, with xxHash64 over the PLAIN encoding of the value, and one bit set per word through salted multiplies (`:46-130`).
- **Sizing table** (`:258-265`): 10.5 bits per distinct value for 1% false positives, 16.9 bits for 0.1%.
- **Placement.** One filter per **column chunk** (`:266-270`). The header is Thrift. The only defined options are UNCOMPRESSED and XXHASH (thrift:803-842).
- **arrow-rs sizing.** Bits = `-8·ndv / ln(1 - fpp^(1/8))`, rounded up to a **power of two** and clamped to between 32 B and 128 MiB (`src/bloom_filter/mod.rs:358-377`). Unless set, NDV defaults to the row-group row limit, 1,048,576 (`src/file/properties.rs:56-64`).
  - At the default fpp of 0.05 that gives about 7.2 Mbit, rounded to **1 MiB per filter**.
  - When rows per distinct key is k (for example, spans per trace), the filter is k× larger than it needs to be.
- **Total size.** R × (filtered columns) × (filter size). The filters are scattered after each row group.
- **Lookup cost.** A key lookup probes **one filter per row group**, each a separate ranged read. Object stores cannot merge them because they are separated by the row-group data (§4).

---

## 4. The read path in arrow-rs

**Planning per row group.** The push decoder runs a state machine per row group (`src/arrow/push_decoder/reader_builder/mod.rs:55-89`):

`Start → Filters → WaitingOnFilterData → … → StartData → WaitingOnData → Finished`

Each `ArrowPredicate` in a `RowFilter` (`src/arrow/arrow_reader/filter.rs:29-46,177`) has a `ProjectionMask`. For each predicate, in order, the reader:
1. fetches the predicate's columns, limited to pages that overlap the current `RowSelection`;
2. decodes them;
3. evaluates the predicate into a `BooleanArray`;
4. ANDs the result into the selection (`src/arrow/arrow_reader/read_plan.rs:198-292`).

Only then is the output projection fetched for the surviving selection. Columns used both as predicate and output can be served from a predicate cache (`array_reader/cached_array_reader.rs`, `row_group_cache.rs`); for that cache, the selection is widened to batch boundaries (`src/arrow/in_memory_row_group.rs:94-100`).

DataFusion leaves `pushdown_filters` **off** by default and does not reorder filters (`config.rs:1231,1236`). By default, then, predicates are evaluated after all projected columns are decoded.

**Consequences:**
- **Serial round trips.** Predicates run one after another, and each is a separate I/O round trip per row group. On an object store, *k* predicates on different columns mean *k*+1 dependent GET waves per row group.
- **No predicate index.** The selection is built only by decoding predicate columns. No structure answers "which rows have status = error" without decoding that column for every candidate row of the row group.
- **Selection representation.** A selection is either a run list (`RowSelector{skip,row_count}`) or a bitmask. The `Auto` policy (threshold 32, `selection/cursor.rs:36-50`) picks the mask when the average run is shorter than 32 rows (`selection/mod.rs:245-292`).

**What forces full-page work:**
- **One row means one page.** `scan_ranges` maps a selection to whole pages through the OffsetIndex (`selection/mod.rs:369-379`, `selection/ranges.rs:33-80`). A single selected row means fetching and **decompressing the entire page**.
- **Skipping inside a page is O(skipped).** `skip_records` (`src/column/reader.rs:312-400`) drops whole pages only when they are fully unselected and their row count is known. Otherwise it decodes the levels and calls `skip_values`, which is O(skipped) for PLAIN byte arrays and DELTA (§2).
- **The dictionary page is always read.** `skip_records` reads it (`column/reader.rs:322-325`), and whenever the page index is used, a range for the dictionary page is added (`in_memory_row_group.rs:84-91`).

**Point lookup of N contiguous rows on a sorted key**, over K projected columns, cold:
1. the footer;
2. one Bloom probe per row group, if Bloom filters are used;
3. the page index for **all** C columns;
4. per column, one data page plus its dictionary page.

That is roughly 4 dependent round trips and about 2K ranges, followed by decompression and decode of K full pages:
- a light INT64 page at the 20k-row cap is 160 KB of values, decoded to keep N rows;
- a 1 MiB page of 10–100 KB strings holds only 10–100 rows, all decompressed.

The CPU cost is modest. The I/O amplification and latency dominate.

**I/O coalescing** (object store):
- `AsyncFileReader::get_byte_ranges` (`src/arrow/async_reader/mod.rs:113-216`) is the entry point.
- `ParquetObjectReader` calls `object_store::get_ranges`, which merges ranges less than **1 MiB** apart and runs **10** fetches in parallel (`object_store-0.14.2/src/util.rs:86-113`).
- arrow-rs itself "does not make any effort to combine consecutive ranges" (`selection/mod.rs:359-366`).
- A column's chunks in different row groups are never adjacent. A narrow projection over many row groups is therefore at least R × K ranges, and on local disk at least R × K seeks or reads.

---

## 5. Structural weaknesses, by workload class

1. **(C) Large repetitive strings are compressed one page at a time.**
   - Each page is an independent codec frame (`Compression.md:31-35`). zstd at level 3 has a window of about 2 MiB, but the frame ends at the page, so only repetition **within one page** is exploited.
   - LLM-trace payloads are cumulative: turn *n* repeats turns 1…*n*−1, and system prompts or tool schemas repeat across thousands of rows. With 10–100 KB values and 1 MiB pages, a page holds 10–100 values. Every page boundary restarts the compressor, so the first copy of a shared prefix is paid in full on every page.
   - Repetition across rows that land in different pages is never captured.
   - Parquet has no shared or trained dictionary, no content-addressed dedup of values, and no delta against a reference value. DELTA_BYTE_ARRAY handles **prefixes only**, and only against the immediately preceding value (`Encodings.md:345-365`).
   - The experimental CDC option in arrow-rs (`src/file/properties.rs:82-123`) aligns page boundaries to content, for dedup *across files* in a CAS layer. It does nothing within a file.
   - For conversation-shaped data, dedup by chunk or reference, followed by compression, should save a factor that grows with conversation length.

2. **(A, D) Row groups couple light and heavy columns.**
   - Every column shares the same row ranges. With a byte cap on row groups (`max_row_group_bytes`; `in_progress_size` sums the compressed pages, `src/column/writer/mod.rs:755-763`), the heavy string columns fill the budget and the light columns are cut into many small chunks. Each chunk adds a footer entry, page-index entries, possibly a dictionary page, and a separate I/O range.
   - Without a byte cap (the default), a row group of 1M rows of 10 KB strings is about 10 GB, fully buffered by the writer. Light-column pruning is then limited to R ≈ 1 interval per file.
   - The format has no per-column row-group size. Lance and Nimble decouple this.

3. **(A, C) Nested maps and lists are encoded with repetition and definition levels.**
   - A `MAP` (`fmt/LogicalTypes.md:1021`) stores a repetition and a definition level for every entry, in both the key leaf and the value leaf. arrow-rs materialises those levels as `i16` vectors (`src/arrow/record_reader/definition_levels.rs`) and rebuilds offsets in scalar loops (`src/arrow/array_reader/list_array.rs:137-210`).
   - A predicate `attrs['k'] = 'v'` must decode every key and every value of each candidate row. No statistics or Bloom filter exist per key.
   - Variant shredding (`fmt/VariantShredding.md:39-80`) is the spec's answer: hot paths become typed columns. That needs Variant support from both the writer and the query engine.
   - With v1 pages, the levels of lists and maps cannot be read without decompressing the values.

4. **(A, E) Wide schemas make metadata grow linearly.**
   - Footer, page index and per-chunk work all scale with R × C (§1).
   - The page index is fetched and decoded for every column, not just the projected ones (`push_decoder.rs:485-499`, `parser.rs:244-320`).
   - Each page header is parsed through Thrift (`serialized_reader.rs:747-846`).

5. **(B, E) Point lookups have coarse random access.**
   - The smallest readable unit is a compressed page (up to 20k rows or 1 MiB under the defaults) plus its dictionary page.
   - There is no offset table inside a page (except ALP's per-vector offsets) and no seekable compression frame. Below the row group, the only row-to-position map is `first_row_index`.
   - Bloom filters exist per row group and are scattered through the file; no file-level key index exists.
   - A cold lookup is about 4 dependent round trips per candidate file. That is negligible on local NVMe and 50–200 ms on S3.

6. **(C) There is no substring or full-text support.**
   - For text, the only metadata is min/max truncated to 64 B (`properties.rs:54,66`). There is no n-gram index, no token Bloom filter and no FSST (still a draft). The page index cannot express "contains".
   - A substring search must fetch and decompress the entire text column. zstd decodes at roughly 1–2 GB/s per core, then come PLAIN decode, UTF-8 validation, optionally a copy, and finally the match. That costs seconds of CPU per compressed GB.
   - FSST would allow matching on compressed strings. Block-level n-gram skip indexes, such as ClickHouse's `ngrambf_v1`, would let a search skip blocks.

7. **(A, D) Numeric columns lack lightweight encodings by default.**
   - Integers and floats default to PLAIN plus a general-purpose codec. ALP is not in arrow-rs, and BYTE_STREAM_SPLIT and DELTA are opt-in.
   - Decoding means a full codec pass plus a memcpy.
   - FastLanes-style FOR + bit-packing, and delta coding over sorted time, decode at memory bandwidth and allow SIMD predicate evaluation directly on packed data.
   - DELTA's prefix-sum dependency and RLE's run headers both resist SIMD and random access.

8. **(D) Writing and compaction are expensive.**
   - The writer buffers each row group in full. `AfterRowGroup` Bloom filter placement trades read locality for writer memory (`properties.rs:173-184`).
   - Merging sorted files means decoding and re-encoding every page. A page cannot be copied between files, because its offsets, its dictionary and its statistics are tied to its chunk.
   - Dictionaries are per chunk, so the same small dictionaries are rewritten in every row group.

9. **(E) Robustness.**
   - A corrupt footer loses the whole file (`fmt/README.md:230-243`).
   - Page CRCs are optional, and the metadata has no checksum. The proposed FlatBuffers extension adds both (`BinaryProtocolExtensions.md:57-110`).

---

## 6. What Parquet does well and a replacement must keep

- **Self-describing, immutable files written in one pass.** The footer comes last, so a writer can stream the file and publish it atomically.
- **Byte-range addressing per column chunk.** A reader fetches only the columns and rows it needs, and the same model works on local disk and object stores.
- **Indexes separate from data.** The page index lives outside the footer, so full scans never pay for it (`PageIndex.md:44-50`). Optional structures are lazy and skippable.
- **Explicit order metadata.** `sorting_columns` (thrift:1078) and `column_orders` let engines exploit sort order for merges and lookups.
- **Nearly free nulls.** Nulls are RLE definition levels, not stored values (`fmt/README.md:177-182`). This matters for sparse metric columns.
- **Per-column, per-page choice of encoding and codec**, with dictionary fallback.
- **Split-block Bloom filters as an algorithm.** One cache line per probe and a clear sizing model. The problem is where they are placed, not the algorithm.
- **Forward compatibility.** Unknown fields are skipped, unknown logical types are handled, and there is an extension escape hatch. Any new metadata encoding must keep this property.
- **Arrow-native type mapping and ecosystem reach.** A successor should map 1:1 to Arrow (including zero-copy `Utf8View`) and be able to export Parquet losslessly.
- **Page CRCs and row-aligned pages**, which make partial reads safe.

---

## Ranked ideas for a faster format

1. **Adopt** content-addressed chunk dedup for large string columns: repeated prompts and prefixes are stored once per file and rows hold references. Compress the unique chunks afterwards.
2. **Adopt** a row-group or block size chosen per column, or a separate region for heavy columns: light columns in large contiguous blocks, heavy columns in their own row-aligned blocks.
3. **Adopt** a footer that needs no parsing (FlatBuffers or a fixed layout): O(1) access to (column, block) metadata, a checksum, and per-column lazy sections. **Avoid** decoding Thrift over all R × C chunks.
4. **Adopt** a file-level sorted key index (key → row range), or one contiguous Bloom section per file, so a cold point lookup costs at most 2 ranged reads after the footer. **Avoid** Bloom filters scattered per row group.
5. **Adopt** small blocks that decompress independently, located through an offset table (for example 64–256 KB or 1–4k rows, like ALP's vector offsets). **Avoid** decompressing 1 MiB pages to read one row.
6. **Adopt** a text skip index per block (n-gram or trigram Bloom filters, or posting bitmaps), optionally with FSST to match on compressed strings.
7. **Adopt** lightweight numeric encodings that decode with SIMD (FastLanes FOR + bit-packing, delta coding of sorted time with block restarts, ALP for decimals-as-floats), and evaluate predicates on encoded data. **Avoid** prefix-sum-only DELTA without block-level random access.
8. **Adopt** shredding of string maps: hot keys become typed columns and the cold rest stays as key-id plus value, with statistics and Bloom filters per key. **Avoid** repetition/definition levels for maps.
9. **Adopt** separate streams for offsets and lengths, with nulls and offsets readable without decompressing the values. **Avoid** PLAIN byte arrays with interleaved lengths, and v1 pages that compress levels together with values.
10. **Adopt** file-level dictionaries for low-cardinality columns, with zone maps over dictionary ids. **Avoid** a dictionary page per chunk that every page read must fetch.
11. **Adopt** predicate evaluation that is index-driven and parallel, resolving in one I/O wave. **Avoid** serial predicates that cost one round trip each per row group.
12. **Adopt** I/O planning inside the format: ranges sorted and merged per file, with light columns laid out next to each other so they coalesce on object stores.
13. **Adopt** block copy-through in compaction: reuse encoded blocks unchanged when merging sorted runs.
14. **Keep** single-pass immutable files, row-aligned blocks, CRCs, explicit sort metadata, cheap nulls, the split-block Bloom algorithm, and lossless export to Arrow and Parquet.
15. **Avoid** relying on truncated min/max for unsorted or high-cardinality columns. Keep them for sorted or clustered keys, such as time and IDs.
