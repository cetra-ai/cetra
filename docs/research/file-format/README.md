# A faster columnar format: Parquet vs Vortex, and a design that beats both

Date: 2026-11-15. Status: research. No decision is taken here, and nothing in the product depends on it.

**Question.** Is there a file format faster than Parquet for LLM-observability data, and if not, what would one look like?

**Scope.** This research is independent of the cetra storage spec. Formats are judged as general-purpose columnar files on five workload classes:
- **(A)** filtered scans over narrow columns;
- **(B)** point lookups by a sorted key;
- **(C)** large, repetitive strings and substring search over them;
- **(D)** write throughput and file size;
- **(E)** local disk versus object store.

LLM-trace data is the example dataset because it stresses all five.

| File | Content |
|---|---|
| [parquet.md](parquet.md) | Deep review of the Parquet format (HEAD `ec90dd4`) and arrow-rs `parquet` 59.3.2, with file:line citations |
| [vortex.md](vortex.md) | Deep review of Vortex (HEAD `d7aa24e8f`, ≈ 0.93.0), with file:line citations |
| [bench/](bench/) | The benchmark crate (data generator, both formats through DataFusion 55, payload-layer prototype) |
| [other-formats-a.md](other-formats-a.md) | Ideas from media containers, compression containers, dedup and VCS tools, and HTTP-range-native formats |
| [other-formats-b.md](other-formats-b.md) | Ideas from LSM and embedded databases, columnar engines, search and log engines, and time-series formats |
| [prototype-plan.md](prototype-plan.md) | Plan for the Strata v0 prototype: design decisions, phases, benchmark protocol, go/no-go criteria |
| this file | Method, measurements, conclusions, and the proposed design |

## TL;DR

1. **Vortex beats Parquet on narrow-column scans.** Through the same DataFusion 55 engine it is 2–7× faster on filtered scans, aggregates and top-k lists. Its compact mode makes files about 10% smaller than the best Parquet configuration, and it writes 2–3× faster.
2. **Vortex is much worse for point lookups and wide values.**
   - A single-trace lookup reads **66–80 MB** from Vortex files, against **2–7 MB** from Parquet. Vortex reads at least a whole segment per column, segments of variable-width columns are 8192 rows long whatever the value size, and its I/O coalescing pulls in segments that were pruned.
   - On S3 that difference costs seconds per lookup.
   - Vortex also cannot store `FixedSizeBinary` (our 16-byte IDs), and cannot push down `map['key']` (the query errors).
3. **Neither format handles large repetitive text well.** Both compress each page or segment on its own, so a system prompt repeated in every row is stored again in every page. Neither has a text index, so `%needle%` decompresses the whole column.
4. **A payload layer fixes that.** It uses content-defined-chunk (CDC) dedup, small zstd frames and a per-frame trigram filter. It was measured as a single-core microbenchmark of that *layer*, not of a complete file format, against Parquet-style 1 MiB zstd pages.
   - **Real agent data** (2.08 GB from 47,198 LLM calls in 1,674 public SWE-agent trajectories):
     - payloads are **38–64% smaller** (17.1 / 12.4 / 10.1 MiB for 64 KiB / 256 KiB / 1 MiB frames, against 27.8 MiB);
     - point reads touch **2.6× fewer bytes** (13 KiB against 34 KiB);
     - substring search is **4–13× faster**: 33–108 ms against 437 ms. Dedup alone helps because only unique chunks are scanned; the trigram filter adds frame skipping. An absent needle reads 0 frames.
   - **Synthetic data** (3.2 GB): 11–35% smaller, point reads 7× faster, search ~9.5× faster.
   - The real data is far more repetitive than the generator: every agent step resends the whole history, so 1 MiB zstd pages already reach 75×, and dedup reaches 122–206×.
5. **Recommendation: a two-layer format.**
   - Narrow columns follow the Vortex model: cascading lightweight encodings, compute on compressed data, and byte-sized blocks.
   - Wide values go to a separate payload layer with dedup, small frames and a skip index.
   - The format adds a zero-parse footer, a sort-key index, one contiguous bloom section and shredded maps.
   - The design is below; the measured and projected gains are in § Expected gains.
   - Nothing in this research says to move off Parquet now. Today only the payload layer and the two existing formats are measured. That a full format built this way would beat both on every workload class is a projection, to be confirmed by an end-to-end prototype (§ Next steps).

## Method

**Data** ([bench/src/datagen.rs](bench/src/datagen.rs)). Synthetic LLM-agent traces with a fixed seed:
- 40 "apps", each with a system prompt of 300–2,200 words (Zipf vocabulary of 6,000 pseudo-words) and 0–6 tool schemas.
- Each trace has a root agent span, then 1–5 LLM calls whose `input` is the growing conversation (OTel GenAI message JSON), with tool calls and tool results in between. Tool results are drawn from a shared corpus of 3,000 documents with a Zipf-like popularity.
- Attribute and resource maps carry realistic keys; nullable token counts and costs; 3% errors with exception events.
- 16-byte trace IDs and 8-byte span IDs.
- Within a file, rows are sorted by `(trace_id, start_time)`; files are time slices.

The generator does **not** model history repeated across the traces of one chat session. The real-data run below shows the generator under-states repetition.

**Real data** (payload layer only).
- Source: two shards (`train-00000`, `train-00001`, 176 MB) of the public [nebius/SWE-agent-trajectories](https://huggingface.co/datasets/nebius/SWE-agent-trajectories) dataset, CC-BY-4.0, downloaded locally and not committed.
- Each assistant turn of a trajectory becomes one LLM span, as an instrumented agent would emit it:
  - `input` is every earlier message as GenAI message JSON;
  - `system_instructions` is the system prompt;
  - `output` is the reply.
- Rows of one trajectory are adjacent, as in a trace-sorted file. Values are cut into 4 column chunks ("files") of about 512 MiB of raw `input` each.
- Built by `fmtbench swe` ([bench/src/main.rs](bench/src/main.rs)).

**Datasets.**

| Name | Traces | Spans | In Arrow | Payload text | Files |
|---|---|---|---|---|---|
| *payload* | 60,000 | 318,139 | 3.8 GB | 3.2 GB | 4 |
| *light* | 300,000 | 1,590,902 | 2.0 GB | none (payload columns null, previews cut to 64 chars) | 4 |

**Variants.**

| Variant | Settings |
|---|---|
| `pq_default` | arrow-rs `WriterProperties` defaults + zstd-3: v1 pages, dictionary on, 1 MiB pages, 1M-row row groups |
| `pq_tuned` | "Point-lookup tuned": v2 pages, 64 KiB / 8,192-row pages, 128k-row row groups, page statistics, bloom filters on the ID columns, dictionary off for large strings |
| `pq_cetra` | Today's cetra writer settings, as a reference point only |
| `vx_default` | Vortex `write_options()` defaults |
| `vx_compact` | Vortex with `BtrBlocksCompressorBuilder::with_compact()` (zstd for strings, pco for numbers) |

Vortex has no `FixedSizeBinary`, so the IDs are cast to `Binary` for Vortex only.

**Engine.**
- DataFusion 55.2 `ListingTable` over each variant: `ParquetFormat` with filter pushdown and reordering on, and `vortex_datafusion::VortexFormat` (Vortex at HEAD `d7aa24e8f`).
- All reads go through a counting `ObjectStore` wrapper, so every query also reports the requests and bytes it would cost on S3.
- Latency is the median of 5 runs (each trace-lookup run averages 20 lookups).

**Machine and caveats.**
- AMD Ryzen 9 3950X (16 cores / 32 threads), 62 GB RAM. Files sit on tmpfs, so these are warm, CPU-bound numbers. The I/O columns are the proxy for cold and S3 cost.
- The Parquet writer here is single-threaded (one `ArrowWriter` per file, files written in sequence), while the Vortex writer uses every core. The write times therefore show the libraries as shipped, not per-core efficiency.
- The payload prototype is single-threaded and per column chunk.

## Results

### Size and write time

*Payload* dataset, 3.8 GB in Arrow:

| Variant | Size | Write time |
|---|---|---|
| pq_default | 332 MiB | 6.7 s |
| pq_tuned | 622 MiB | 11.3 s |
| pq_cetra | 438 MiB | 8.9 s |
| vx_default | 487 MiB | 4.2 s |
| vx_compact | **316 MiB** | **4.0 s** |

*Light* dataset, 2.0 GB in Arrow:

| Variant | Size | Write time |
|---|---|---|
| pq_default | 151 MiB | 5.2 s |
| pq_tuned | 191 MiB | 5.3 s |
| pq_cetra | 161 MiB | 4.8 s |
| vx_default | 210 MiB | **1.6 s** |
| vx_compact | **136 MiB** | 1.8 s |

What drives these sizes:
- `pq_tuned` is the largest because 64 KiB pages compress large text in small independent frames: the random-access versus ratio trade-off, measured.
- `pq_default` is small mostly because default dictionary encoding catches the exactly repeated system prompts.
- Vortex default mode (FSST, OnPair, dict) is weak on long text. Compact mode adds zstd and wins.

### (A) Filtered scans over narrow columns

Median ms on the *light* dataset (1.59M spans):

| Query | pq_default | pq_tuned | vx_default | vx_compact |
|---|---|---|---|---|
| `count(*)` where `status='error'` and 6 h window | 15.7 | 9.5 | **5.5** | 5.5 |
| Group by model over the day, 4 aggregates | 40.6 | 23.7 | **13.3** | 16.3 |
| Top 50 LLM spans in 1 h, 10 columns, `ORDER BY start_time DESC` | 69.0 | 26.8 | **10.4** | 14.3 |
| `user_id = 'user-1234'` (bloom / zone) | 8.6 | 5.9 | **4.3** | 7.3 |
| `metadata['customer_id'] = …` (map key) | 37.2 | **18.7** | error | error |

On the *payload* dataset the ordering is the same, and so are the ratios: list 28 → 6 ms, rollup 15 → 7 ms, count 7.5 → 2.5 ms.

- Vortex is **2–7× faster than default Parquet and 1.4–2.6× faster than tuned Parquet**, while issuing 5–60× fewer requests.
- The map-key filter fails on Vortex: DataFusion gets back `Utf8View` values and compares them to a `Utf8` literal. The Vortex converter does not support `get_field` on Map (vortex.md §4).

### (B) Point lookup of one trace

Per lookup, *payload* dataset. "Full" is `SELECT *`; "light" is 9 narrow columns.

| Variant | Full: ms | Full: bytes read | Full: requests | Light: ms | Light: bytes read |
|---|---|---|---|---|---|
| pq_default | 26.4 | 6.3 MB | 112 | 7.0 | 1.7 MB |
| pq_tuned | **12.7** | 3.4 MB | 101 | 7.1 | 2.3 MB |
| pq_cetra | 17.0 | **2.6 MB** | 74 | 6.1 | **0.56 MB** |
| vx_default | 16.1 | **75.9 MB** | 12 | **4.2** | 17.8 MB |
| vx_compact | 31.6 | 79.9 MB | 11 | 4.2 | 11.4 MB |

On the *light* dataset Vortex still reads **77 MB** per full lookup, about 37% of all the data, against **5.1 MB** for tuned Parquet.

Warm latency hides this; I/O does not. On S3 at about 100 MB/s per stream, 76 MB is close to a second per lookup, against tens of milliseconds for Parquet. The cause is in vortex.md §4–§6:
- there are no sub-segment reads;
- segments of variable-width columns are 8192 rows long whatever the value size;
- coalescing merges pruned segments within 1 MiB.

Parquet's cost, in turn, is the request count: about 100 dependent ranged reads per lookup (page index, then one or more pages per column), plus whole-page decompression.

### (C) Large strings: scan and substring search

*Payload* dataset; `count(*)` where `input LIKE '%zqxjneedle%'` (16 hits), and a full `sum(length(input)) + sum(length(output))`:

| Variant | LIKE: ms | LIKE: bytes read | Full scan: ms | Full scan: bytes read |
|---|---|---|---|---|
| pq_default | 191 | 125 MB | 335 | 258 MB |
| pq_tuned | 231 | 160 MB | 427 | 327 MB |
| pq_cetra | 137 | 125 MB | 246 | 258 MB |
| vx_default | 136 | 236 MB | 210 | 405 MB |
| vx_compact | **93** | 113 MB | **162** | 253 MB |

Both formats read and decompress the whole column for a substring search, because neither has a text index.

### The payload-layer prototype

[bench/src/payload.rs](bench/src/payload.rs). Single core, all four payload columns (`input`, `output`, `system_instructions`, `tool_definitions`): 3.2 GB in total.

- Lookup figures are averages over 200 random rows per column chunk.
- Search is the rare needle (16 hits); hit counts are checked equal across schemes.

| Scheme | Stored | Ratio | Encode | Full decode | Point read: bytes | Point read: time | Rare-needle search | Frames read |
|---|---|---|---|---|---|---|---|---|
| 1 MiB pages, zstd-3 (Parquet-like) | 398 MiB | 8.1× | 522 MB/s | 2,585 MB/s | 153 KiB | 492 µs | 1,382 ms | 3,233 / 3,233 |
| 64 KiB blocks, zstd-3 | 659 MiB | 4.9× | 364 MB/s | 1,617 MB/s | 14 KiB | 43 µs | 2,171 ms | all |
| 64 KiB blocks, zstd-3 + trained dict | 441 MiB | 7.3× | 210 MB/s | 1,542 MB/s | 10 KiB | 45 µs | 2,255 ms | all |
| CDC dedup, 64 KiB frames | **322 MiB** | 10.0× | 414 MB/s | 1,566 MB/s | **21 KiB** | **64 µs** | 2,074 ms | all |
| CDC dedup, 256 KiB frames | 296 MiB | 10.8× | 435 MB/s | 1,449 MB/s | 68 KiB | 231 µs | 1,544 ms | all |
| CDC dedup, 1 MiB frames | **261 MiB** | **12.3×** | 469 MB/s | 2,649 MB/s | 196 KiB | 595 µs | 1,231 ms | all |
| CDC dedup, 256 KiB frames + trigram filter (8 KiB/frame) | 328 MiB | 9.8× | 452 MB/s | 1,717 MB/s | 68 KiB | 229 µs | **242 ms** | **16 / 4,101** |
| **CDC dedup, 64 KiB frames + trigram filter (2 KiB/frame)** † | 355 MiB | 9.1× | 287 MB/s | 2,852 MB/s | **21 KiB** | **67 µs** | **144 ms** | **16 / 16,629** |

† Measured in a second run. In that run 1 MiB pages took 1,372 ms for search and 477 µs per point read, and CDC encode ran at 287–318 MB/s. Run-to-run spread is about ±15% on encode and decode throughput; sizes and bytes read are deterministic.

Per column, CDC dedup is decisive on values that repeat far apart:

| Column | 1 MiB pages | CDC dedup |
|---|---|---|
| `system_instructions` (1.45 GB) | 120 MiB | **0.7 MiB** |
| `tool_definitions` | 10.7 MiB | **0.2 MiB** |
| `input` (conversation, repeated close together) | 130 MiB | 127–160 MiB, depending on frame size |

On `input`, 1 MiB zstd pages already capture the repetition, because the rows of a trace sit next to each other.

How to read the table:
- Dedup is what wins on size.
- Small frames are what win on point reads.
- The trigram filter is what wins on search, for selective needles. A needle present in most frames still reads them all, though over unique chunks only, so it is never worse than without the filter.
- A trained zstd dictionary recovers most of the ratio that small frames lose, at half the encode speed.

### The payload layer on real agent data

1,674 SWE-agent trajectories give 47,198 LLM calls and 2.08 GB of payload, almost all of it `input`. The table covers all columns:
- the rare needle is the 20-byte string `example in appendix ` (951 rows);
- the absent needle is `zqxjneedle` (0 rows).

| Scheme | Stored | Ratio | Point read | Rare needle: ms | Rare needle: frames read | Absent needle: ms | Absent needle: frames read |
|---|---|---|---|---|---|---|---|
| 1 MiB pages, zstd-3 (Parquet-like) | 27.8 MiB | 75× | 34 KiB | 437 | 2,163 / 2,163 | 226 | 2,163 / 2,163 |
| 64 KiB blocks, zstd-3 | 248.5 MiB | 8.4× | 4.7 KiB | 2,516 | all | 872 | all |
| 64 KiB blocks + trained dict | 164.9 MiB | 12.6× | 3.7 KiB | 1,610 | all | 1,199 | all |
| CDC dedup, 64 KiB frames | **17.1 MiB** | 122× | **13 KiB** | 108 | 2,536 / 2,536 | 260 | all |
| CDC dedup, 256 KiB frames | 12.4 MiB | 168× | 32 KiB | 170 | 626 / 626 | 234 | all |
| CDC dedup, 1 MiB frames | **10.1 MiB** | **206×** | 87 KiB | 117 | 161 / 161 | 155 | all |
| CDC, 256 KiB frames + trigram filter (8 KiB/frame) | 17.3 MiB | 120× | 32 KiB | **33** | **15 / 626** | 51 | **0 / 626** |
| CDC, 64 KiB frames + trigram filter (2 KiB/frame) | 22.1 MiB | 94× | 13 KiB | 52 | 170 / 2,536 | **35** | **0 / 2,536** |

What the real data shows:
- **Agent payloads are extremely repetitive.** Each step resends the whole conversation, so even page-local zstd reaches 75×.
  - Small independent blocks lose almost all of it: 64 KiB blocks reach only 8.4×. A format that wants cheap point reads cannot simply shrink its pages (the `pq_tuned` lesson again).
  - Dedup keeps small frames *and* improves the ratio, because it removes the repeats before compressing.
- **Search gets faster even without a filter.** Only unique chunks are scanned, so the search itself runs 4× faster. The trigram filter then skips 97–100% of frames.
- **The filter has to be sized against compressed data.** At 2 KiB per 64 KiB raw frame it adds 5 MiB to a 17 MiB column. A writer should size filters from each frame's compressed size or unique-trigram count, not its raw size.
- **Timings are noisy (±2× between passes on a shared machine).** Sizes, bytes and frame counts are deterministic. Search times are from the first pass; the absent-needle pass ran separately.

## What each format gets right and wrong

The full analysis is in [parquet.md](parquet.md) and [vortex.md](vortex.md).

**Parquet**

Keep:
- immutable single-pass files;
- byte-range addressing;
- statistics and page index outside the data;
- sort metadata;
- nulls that cost nothing;
- split-block bloom filters (the algorithm);
- ecosystem and export.

Fix:
- **Footer.** The Thrift footer is decoded in full, O(row groups × leaf columns), with no checksum.
- **Page index and blooms.** The page index is fetched for every column, projected or not. Bloom filters sit after each row group, so a cold lookup costs one read per row group.
- **Predicates.** They are evaluated serially, one I/O wave per predicate per row group.
- **Pages.** A single row read decompresses a whole page.
- **Encodings.**
  - Byte arrays fall back to PLAIN v1, with lengths interleaved.
  - arrow-rs has neither ALP nor FSST.
  - Row groups tie narrow and wide columns to the same row ranges.
  - Maps go through rep/def levels with one min/max across all keys.
- **Text search.** There is no text index.

**Vortex**

Keep:
- BtrBlocks-style cascading encoding chosen by sampling;
- FastLanes, ALP and FSST/OnPair;
- kernels that run on compressed data;
- three-phase scans that never decode all-false chunks;
- a postscript readable in one tail read, with an injectable footer cache;
- segment I/O with lazy coalescing;
- editions for wire compatibility.

Fix:
- **Segments.** Variable-width segments have a fixed row count, and there are no sub-segment reads.
- **Coalescing.** Pruned segments get coalesced into reads.
- **Long text.** No long-range compression in default mode; zstd is at most per-segment.
- **Lookups.** No key index; the bloom filter is unstable.
- **Maps.** Map is an opaque leaf with no key pushdown.
- **Types.** No `FixedSizeBinary`, `Duration` or `Interval`.
- **Footer.** One monolithic layout flatbuffer.
- **Writer memory.** Unbounded per column.
- **Maturity.**
  - The Rust API is unstable, with weekly 0.x releases.
  - About 1,100 `unsafe` blocks.
  - Some writer output depends on an environment variable.

## Proposed design: a two-layer format

Working name **Strata**. A general-purpose columnar file format for append-mostly event data. It mixes narrow typed fields (IDs, timestamps, enums, counters) with wide, repetitive text (prompts, logs, JSON documents). It takes what Vortex does best for narrow columns and adds a separate storage layer for large values, which neither Parquet nor Vortex has.

### Principles

1. **Two physical layers, one logical schema.** Narrow columns and wide values have opposite needs. Narrow columns want large blocks with lightweight SIMD encodings. Wide values want dedup, long-range compression and small random-access frames. Parquet's row group ties the two together (the wide columns decide where row groups close). Vortex treats both alike (8192-row segments of 100+ MB for large strings). Strata decides per column, at write time, from the observed average value size (default threshold 256 B).
2. **Size every unit in bytes, never in rows.** Blocks of narrow columns target 64–256 KiB compressed. Payload frames target 64 KiB raw.
3. **Never pay for what a query does not touch.** The footer needs no parsing and gives O(1) access to any (column, block) entry. Indexes live in separate lazy sections. One tail read normally opens a file.
4. **Every point lookup takes at most 3 dependent reads:** tail/footer (usually cached), key index, data. All data reads go out in a single wave.

### File layout

```
'STRA' v1
[block segments ...]            narrow-column blocks, ordered by (row range, column)
[frame segments ...]            payload frames, ordered by first reference
[index sections ...]            key index, bloom section, trigram filters, edge tables
[column directory]              fixed-width records: per column → offsets of its block table, stats, dict
[schema]                        Arrow schema (IPC flatbuffer), lossless incl. FixedSizeBinary, Map, Dictionary
[postscript 64 B]               offsets+lengths+xxh3 of directory, schema, sections; format version; feature bits
'STRA'
```

- **Column directory.** One fixed-size record per column, so reading column *c* costs `dir + c × rec_size` with no parsing. Each record points to the column's **block table**: fixed-width rows `{first_row u32, rows u32, offset u64, len u32, encoding u16, zone_min/max (inline ≤ 16 B, or a pointer), null_count u32}`. Opening a file for a 5-column query decodes 5 small arrays, not C × B Thrift structs. This fixes the cost Parquet's metadata decode grows with (row groups × leaf columns) and Vortex's single layout flatbuffer.
- **Checksums everywhere.** xxh3-64 on every segment, the directory and the postscript. Parquet's page CRC is optional and its footer has none.
- **Feature bits plus editions.** Readers refuse unknown *required* features and skip unknown *optional* sections (Vortex's edition model, without behaviour that depends on environment variables).

### Narrow columns: the Vortex model, minus its rough edges

- Cascading per-block encoding chosen by sampling: FoR/bit-packing (FastLanes layout), delta for sorted columns, ALP/ALP-RD for floats, RLE/run-end, sparse, constant, sequence, dictionary. FSST or OnPair for short strings.
- **File-level dictionaries** for low-cardinality strings (one dictionary per column per file, stored once). Every block stores only codes. Zone maps then hold code ranges, and `status = 'error'` becomes an integer compare on packed codes.
- **Compute on encoded data:** compare, between and IN on bit-packed and dictionary codes, `LIKE` on dictionary values or on FSST through its DFA. Filter, then take with a selection mask, with no decode of blocks that are entirely false.
- **Zone maps** per block (min/max truncated to 32 B for strings, null_count, distinct-count estimate). Columns that are sorted or clustered get exact pruning; the others rely on the indexes below.
- **Lossless Arrow mapping.** FixedSizeBinary (16-byte trace IDs, 8-byte span IDs) is stored as a fixed-width primitive (plain bytes plus zone maps). Map, Dictionary, Duration and Union round-trip.

### Wide values: the payload layer

This layer was measured by the prototype in `bench/src/payload.rs`.

1. **Content-defined chunking.** A gear hash cuts chunks of min 512 B, average about 2 KiB, max 8 KiB.
2. **File-wide dedup** by xxh3-128. Identical values and shared sub-documents (system prompts, tool schemas, repeated conversation history, retrieved documents) are stored once per file.
3. **Unique chunks are packed into 64 KiB frames**, each compressed with zstd-3 on its own. An optional per-column trained zstd dictionary is stored once.
4. **Rows are lists of chunk IDs**, delta-varint encoded and zstd-compressed, plus a chunk table `{frame, offset, len}`.
5. **Search sidecars:**
   - A per-frame trigram filter (2 KiB per 64 KiB frame, about 3%). A `%needle%` search decompresses only the frames whose filter matches every trigram of the needle.
   - A chunk edge table (the first and last 63 B of each chunk) finds needles that cross chunk boundaries without decompressing anything.
   - Search runs over *unique chunks*, so a prompt repeated 10,000 times is scanned once.
6. **Point lookup:** the row's refs, then the few frames they point to (about 20 KiB compressed in the prototype, against about 150 KiB for a 1 MiB page).
7. **Compaction copies frames.** Merging files rewrites only the reference lists and the chunk tables. Frames are copied as bytes, and chunks already present in the output are deduplicated by hash, so the dominant bytes are never decompressed or recompressed. That is a large write and compaction win, because these bytes are most of the data.
8. **Optional cross-file store.** Because chunks are content-addressed, a later version can move frames into a shared per-partition chunk store. Dedup then works across files and days, as Git packfiles or restic do.

### Semi-structured maps

`Map<Utf8, Utf8>` and JSON attribute bags are **shredded at write time**:
- Keys present in at least 5% of a file's rows become typed sub-columns (narrow-layer rules: dictionary, zone maps, bloom).
- The remainder is stored as `(key_code, value)` lists, with a file-level key dictionary.
- `attributes['k'] = v` becomes a pushdown predicate on one sub-column.

Neither Parquet (rep/def levels, one min/max across all keys) nor Vortex (Map is an opaque leaf; `get_field` on Map is unsupported in DataFusion) can push down such a predicate. Parquet's Variant shredding is the same idea, but arrow-rs does not apply it to maps.

### Indexes (lazy, optional, separate sections)

- **Sort-key index.** For the declared sort key: the first key of every block, stored as fixed-width keys or as an Elias–Fano/learned index over a numeric prefix. A key lookup becomes binary search, then a row range, then the blocks.
- **One contiguous bloom section** (split-block bloom filters, Parquet's algorithm) for the columns the writer names, sized from the real distinct count. A cold lookup fetches it with one ranged read per file, not one per row group.
- **Trigram frame filters and edge tables** for payload columns, as described above.

### Read path

- **Three phases:** prune with zone maps and indexes, evaluate filters on encoded filter columns only, then project with the final mask.
- **One planned I/O wave per phase.** The predicate columns of all conjuncts are fetched together (Parquet fetches one wave per predicate per row group).
- **Coalescing depends on the query:**
  - scans merge reads within a 1 MiB gap, up to 16 MiB (S3) or 4 MiB (local);
  - point lookups do not merge across pruned blocks (Vortex's coalescing pulls those in).
- **Zero-copy `Utf8View` output** points into decompressed frames that are cached and shared within the query.
- **DataFusion:** a `FileFormat` plus a `TableProvider`. Pushdown covers comparisons, IN, LIKE/contains (narrow layer: FSST or dictionary; payload layer: trigram filter), map-key access, and `trace_id = X` through the key index.

### Write path

- Streaming and single pass, with bounded memory. The writer keeps one open block per narrow column and one open frame per payload column, plus the dedup hash table (≈ 24 B per unique chunk).
- Encoding parallelism is bounded by a global in-flight byte budget, not by chunks per column (Vortex's per-column parallelism can blow up memory on wide values).

## Expected gains

Each gain is compared with the best measured variant. "Measured" means this benchmark measured it; "projected" means it is derived from a measured mechanism and has not been built.

| Workload | Best today | Two-layer format | Basis |
|---|---|---|---|
| (A) Narrow-column scans | Vortex: 2–7× over Parquet default | Vortex-level; 10–30% better with file-level dictionaries and one I/O wave for all predicates | Vortex measured; the extra gain is projected |
| (A) Map-key filter | Parquet tuned, 18.7 ms (Vortex errors) | Same cost as a narrow-column filter on one shredded column, about 5 ms, 3–4× better | Projected from the `user_filter` timings |
| (B) Trace lookup, bytes read | Parquet, 2.6–3.4 MB in ~100 requests; Vortex, 76 MB in ~12 | **< 0.5 MB in 2–3 dependent waves**: key index → one block per narrow column + ~21 KiB per payload column | Payload part measured (21 KiB/column); narrow part projected from block size |
| (C) Payload size, real data | 1 MiB zstd pages, 27.8 MiB | 17.1 MiB (64 KiB frames) to 10.1 MiB (1 MiB frames): **38–64% smaller** | Measured (prototype) |
| (C) Payload size, synthetic | 1 MiB zstd pages, 398 MiB | 322 MiB (64 KiB frames) to 261 MiB (1 MiB frames): 19–35% smaller | Measured (prototype) |
| (C) Payload point read | 1 MiB pages: 34 KiB (real), 153 KiB (synthetic) | 13 KiB (real), 21 KiB (synthetic): **2.6–7×** fewer bytes | Measured (prototype) |
| (C) Rare substring search | 1 MiB pages, one core: 437 ms (real), 1,372 ms (synthetic) | 33–52 ms (real), 144 ms (synthetic): **~8–13×** less CPU; absent needles read 0 frames | Measured (prototype) |
| (D) Write | Vortex, ~1 GB/s of Arrow with all cores | Narrow layer at Vortex speed; payloads encode at 414–469 MB/s per core, and compaction copies frames instead of recompressing them | Encode measured; compaction projected |
| (E) Object store | Parquet: many small ranges; Vortex: few huge ranges | Few small ranges: one tail read, a zero-parse directory, and coalescing by query type | Projected |

## Risks and costs

- **It is a new storage format.** That means a writer, a reader, a DataFusion integration, fuzzing, crash safety and export. A first narrow layer could reuse Vortex's encoding crates (Apache-2.0) behind our own container, so only the container, the payload layer and the indexes are new code. Their `unsafe` and API churn would then become our dependency risk.
- **Data coverage.** The real-data run covers one workload, coding agents, where every step resends the history. Chat apps repeat history across turns too. One-shot prompts such as RAG Q&A or classification repeat much less, so expect gains closer to the synthetic run, and near zero for unique text. The payload layer is never worse than page zstd on size by more than the filter's overhead.
- **The trigram filter only helps selective needles.** Common words read every frame (still over unique chunks only). Case-insensitive search needs lowercase trigrams.
- **Interop.** Users who want files get a Parquet export path, not the native files.

## Next steps (if pursued)

1. Prototype the payload layer as an Arrow-in / Arrow-out codec with a DataFusion `TableProvider` over one file. Measure end to end against `pq_tuned` and `vx_compact` with the queries above, on S3 with real latency.
2. Add a one-shot workload (for example a RAG or classification dataset) to bound the low end of the gains.
3. Decide whether to reuse Vortex's narrow-column encodings or write our own.

## Reproducing

[bench/](bench/) is a standalone crate (it has its own `[workspace]`). It depends on `/home/sebastien/github/vortex` by path and on `crates/core` for the span types. Building `vortex-io` needs libclang (`LIBCLANG_PATH`), and rustc 1.99.1 needs `RUST_MIN_STACK=33554432` for the release build.

```bash
cargo build --release
./target/release/fmtbench gen <dir> 60000 4      # payload dataset
FMT_LIGHT=1 ./target/release/fmtbench gen <dir> 300000 4
./target/release/fmtbench query <dir> 5
./target/release/fmtbench payload <dir>           # payload-layer prototype
./target/release/fmtbench swe 512 train-00000-of-00012.parquet train-00001-of-00012.parquet  # real data
```
