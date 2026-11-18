# A faster columnar format: Parquet vs Vortex, and a design that beats both

Date: 2026-11-15; Strata v1 and v2 results 2026-11-17. Status: research. No decision is taken here, and nothing in the product depends on it.

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
| [prototype-plan.md](prototype-plan.md) | Plan for the Strata prototype (v0, with the v2 payload layer in § S4 in v2): design decisions, phases, benchmark protocol, go/no-go criteria |
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
6. **The end-to-end prototype, Strata v0, fails its go/no-go: Stop** (§ Strata v0 results). Only 2 of the 11 criteria pass: the no-engine lookup, and the map-key filter.
   - **Bytes:** cold lookups read 0.65–0.73× of the best Parquet's bytes, and substring search 0.66×. Real-data payloads are 0.88× of Parquet's size, against the 0.6× targeted. The prototype's better size counted neither the trigram filters nor the `output` column, which has nothing to deduplicate.
   - **Speed:** narrow scans are 1.2–2.3× slower than Parquet and up to 5× slower than Vortex. Writes run at 0.20× of Parquet's speed, and merges are 2.6× slower. Substring search is 6.5× faster than Parquet.
7. **Strata v1 passes the same go/no-go: Go** (§ Strata v1 results). G1a, G1b, G4 and G6 pass, and only G8 (write speed) and G9 (merge) fail. G7 is not judged, because the search filters are now off by default.
   - **Lookups:** a warm full-trace lookup reads 281–442 KiB, from 2.2× to 5.9× less than the best Parquet. A cold one reads 0.24–0.30× of its bytes, in 4 waves.
   - **Narrow queries:** all four beat the best Parquet by the 0.6× the plan asked for and stay within 1.3× of Vortex compact; `user_filter` beats both.
   - **Size:** real-data payloads are 0.57× of Parquet's size, from one chunk store shared by `input` and `output` and zstd level 9.
   - **Costs:** substring search lost its 6.5× win and now runs at Parquet's speed. Writes run at about 0.16× of Parquet's speed, and merges are 4.8× slower.
8. **Strata v2 passes every criterion: Go** (§ Strata v2 results). All 11 of G1a–G9 pass, G7 included, with search back on.
   - **Lookups:** a warm full-trace lookup reads 260–392 KiB, from 2.4× to 6.7× less than the best Parquet, and takes 4.8 ms against 12.2 ms on `payload`. A cold one reads 0.25× of the best Parquet's bytes, in 4 waves.
   - **Search:** a 5-gram index per frame makes `%needle%` on real agent data 25× faster than Parquet and 10× faster than Vortex compact, reading 0.16× of Parquet's bytes.
   - **Size:** real-data payloads are 0.56× of Parquet's size; `payload` files are 0.78× of pq_default's and 0.82× of vx_compact's.
   - **Costs:** writes run at 0.87× of Parquet's speed and merges at 0.27× of its time. What remains slower: a full payload scan takes 1.3× Parquet's time (2.2× before the last reader change), `light` files are 17% bigger than Parquet's, and Vortex compact is still faster on the aggregate rollup.

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

## Strata v0 results (M7.4)

These figures come from the M7 bench ([bench/](bench/), [prototype-plan.md](prototype-plan.md) § Go / no-go criteria), run on `ubuntu-vm` (12 vCPUs). Each dataset is written as 4 files per variant.

- **Sizes, bytes, requests and waves** come from a run on 2026-11-15, and are the same in every later run. Bytes and requests are counted by a wrapper around the object store.
- **Timings** come from 2026-11-16, with the VM otherwise idle:
  - local disk: 30 runs per query (`swe` needles: 5 runs), then `profile` and `merge`; the load average stayed between 0.1 and 2.4;
  - S3: Versity Gateway v1.11.0 behind toxiproxy 2.13.0, which adds 20 ± 5 ms per response; 5 runs per query, 1 run per needle.
- **Deviation:** S3 uses 5 runs instead of the planned 30. Parquet's no-engine lookups make up to 2,900 requests per trace, so 30 runs would have taken hours per variant. The load rose to about 5 near the end of the S3 `light` run, so treat those figures as indicative.

**Datasets.**
- `payload`: 318,139 synthetic spans (60,000 traces); 3,211 MiB of payload strings.
- `light`: 1,590,902 synthetic spans (300,000 traces), with no payloads.
- `swe`: 94,396 spans from two shards of the public SWE-agent trajectories; 2,155 MiB of payload strings. The bench uses 20 needles of 8–32 bytes, with 15–937 hits each.

### File size (MiB, 4 files)

| Dataset | `st` | pq_default | pq_tuned | pq_cetra | vx_default | vx_compact |
|---|---|---|---|---|---|---|
| payload | 419 | 332 | 622 | 438 | 487 | 316 |
| light | 166 | 151 | 191 | 161 | 210 | 136 |
| swe | 35 | 36 | 65 | 36 | 560 | 31 |

`swe` payload columns, in KiB:
- `input`: Strata 15,180, against 21,289 for pq_default, 45,791 for pq_tuned and 21,288 for pq_cetra.
- `output`: Strata 12,030, against 9,729, 12,394 and 9,791.

Every Strata frame has a trigram filter.

### Trace lookup (mean per trace ID over 20 IDs)

**Warm.** A warm-up pass loads the file metadata first, so these figures count data reads only.

| Dataset | `st` | Best Parquet | Others |
|---|---|---|---|
| payload, all columns | **1,210 KiB**, 39 requests, 2.0 waves | pq_cetra: 2,610 KiB, 74 requests | pq_default: 6,350 KiB; vx_compact: 70,381 KiB |
| payload, light columns | **141 KiB**, 9.5 requests, 1.0 wave | pq_cetra: 563 KiB, 16 requests | |
| swe, all columns | 677 KiB, 15 requests, 2.0 waves | pq_tuned: **615 KiB**, 102 requests | pq_default: 1,328 KiB |
| swe, light columns | **179 KiB**, 4.2 requests, 1.0 wave | pq_cetra: 276 KiB, 14 requests | |

**Cold.** A fresh context for each ID, so the open and the metadata are counted too. Requests exclude HEAD and LIST.

| Dataset | `st` | pq_default | pq_tuned | pq_cetra | vx_compact |
|---|---|---|---|---|---|
| payload | **3,037 KiB**, 68.5 requests, 4 waves | 8,398 KiB, 116 | 9,094 KiB, 109 | 4,658 KiB, 78 | 73,355 KiB, 15 |
| swe | **1,940 KiB**, 28.4 requests, 4 waves | 3,376 KiB, 110 | 2,663 KiB, 106 | 2,843 KiB, 75 | 29,542 KiB, 13 |

The prototype's no-engine Parquet reader (`pq_fast`) reads 10–100 MiB for one full trace. It is a correctness baseline, not a contender.

### Narrow queries and search (KiB fetched, warm)

| Query | Dataset | `st` | pq_default | Best Parquet | vx_compact |
|---|---|---|---|---|---|
| count_errors_6h | light | 4,297 (4 requests) | 5,111 (46) | pq_cetra: 4,300 (52) | 3,954 (16) |
| model_rollup_day | light | 16,252 (20) | 13,007 (340) | 13,007 | 17,729 (19) |
| list_llm_1h | light | 16,553 (7) | 14,292 (214) | 14,271 | 18,859 (8) |
| user_filter | light | 1,606 (11) | 2,089 (16) | pq_cetra: 1,231 (31) | 2,184 (13) |
| metadata_filter (map key) | light | 1,170 (4) | 2,088 (8) | 2,088 | errors |
| substring_input, 1 needle | payload | **8,999** (71) | 128,316 (4) | 128,316 | 115,726 (38) |
| substring, mean over 20 needles, cold | swe | **14,143** | 21,289 | 21,289 | 19,085 |
| scan_all_payload | payload | 321,591 (32) | 263,995 (8) | 263,995 | 263,680 (43) |

On narrow queries, Strata reads as much as Parquet and Vortex, in a few large requests where Parquet makes hundreds of small ones. Over S3 that should favour Strata; the timed S3 run will tell. Its `trace_list` fast path (`st_fast list_llm_1h`) makes one wave per chunk: 80 waves on `payload` and 196 on `light`, against 2–4 through DataFusion. It also reads 4–9× more bytes. It needs a cross-chunk fetch before it is usable over S3.

### Timed results (local disk, min of 30 runs)

**Trace lookup on `payload`, warm, ms per trace:**

| | `st` | `st_fast` | Best Parquet | `pq_fast` (best) | vx_compact |
|---|---|---|---|---|---|
| all columns | 7.15 | 6.02 | pq_tuned: 12.16 | 322.10 | 33.71 |
| light columns | 1.09 | 0.69 | pq_cetra: 5.18 | 8.39 | 5.57 |

**Profile on `payload`, one partition, min of 30, ms.** Total is the whole query, and "execute" is the time in execution, from the plan metrics.

| | `st` total | `st` execute | Best Parquet total | Best Parquet execute |
|---|---|---|---|---|
| lookup, all columns | 7.10 | 6.05 | pq_tuned: 11.37 | 8.90 |
| lookup, 9 light columns | 1.02 | 0.70 | pq_cetra: 4.48 | 2.94 |
| trace list (top 50 in 1 h) | 30.07 | 28.83 | pq_cetra: 24.97 | 22.51 |

**Narrow queries on `light`, ms:**

| Query | `st` | Best Parquet | vx_compact | vx_default |
|---|---|---|---|---|
| count_errors_6h | 12.72 | pq_cetra: 10.63 | 6.16 | 4.29 |
| model_rollup_day | 79.56 | pq_cetra: 38.38 | 19.59 | 14.34 |
| list_llm_1h | 98.51 | pq_tuned: 43.37 | 18.74 | 11.51 |
| user_filter | 11.45 | pq_cetra: 6.99 | 11.76 | 4.71 |
| metadata_filter | 9.65 | pq_tuned: 29.25 | errors | errors |

**Substring search on `swe`, 20 needles, warm, one partition:** `st` takes 79 ms per needle, against 510 ms for pq_cetra and 206 ms for vx_compact.

**Write and merge on `payload`:**
- `gen` writes at 107 MiB/s of Arrow for `st`, against 534 MiB/s for pq_default and 526 MiB/s for vx_compact.
- Merging the 4 files takes 27.3 s for Strata and 10.4 s for Parquet: 2.64× slower.

### S3 with 20 ms of added latency (min of 5 runs, ms)

| Query | Dataset | `st` | pq_default | Best other |
|---|---|---|---|---|
| lookup, warm, all columns | payload | 115.7 | 89.9 | |
| lookup, warm, light columns | payload | 43.9 | 73.4 | |
| lookup, cold, mean | payload | 337.6 | 154.5 | vx_default: 256.7 |
| lookup, warm, all columns | swe | 73.6 | 76.7 | |
| lookup, cold, mean | swe | 265.1 | 141.6 | |
| substring, 20 needles | swe | 315 per needle | 651 | vx_compact: 651 |
| count_errors_6h | light | 44.8 | 69.0 | vx_compact: 165.4 |
| list_llm_1h | light | 172.0 | 210.3 | vx_compact: 168.9 |
| `st_fast` list_llm_1h | light | 9,527 | | |

**Wave counts do not turn into lower latency.** A cold Strata lookup needs 4 waves, which should cost about 100 ms at 25 ms per round trip. It takes 265–338 ms, about twice as long as pq_default, which makes 110 requests. Two plausible causes, neither confirmed:
- each of the 4 files is opened in turn, with a HEAD and a tail read for each;
- the requests in a wave are serialized somewhere between the reader and the HTTP client.

This needs a trace of the requests before any S3 claim is made for the format. The `trace_list` fast path is unusable on S3, at 9.5 s, because it makes one wave per chunk.

### Go / no-go

| # | Criterion | Measured | Verdict |
|---|---|---|---|
| G1a | warm lookup ≤ 0.5 MB, ≤ 2 waves, best Parquet ≥ 2× the bytes | payload: 1.24 MB, 2 waves, Parquet 2.2×; swe: 0.69 MB, 2 waves, Parquet **0.91×** | **fail** (bytes on both; ratio on swe) |
| G1b | cold lookup ≤ 0.5 × best Parquet bytes, ≤ 4 waves | payload 0.65×, swe 0.73×; 4 waves | **fail** |
| G2 | `st_fast` ≤ 0.5 × `pq_fast`, light and all columns | light 0.08×, all 0.02× | pass |
| G3 | `st` total ≤ 0.75× and execute ≤ 0.5× of best Parquet, light and all columns | light: total 0.23×, execute 0.24×; all: total 0.62×, execute **0.68×** | **fail** (execute, all columns) |
| G4 | each narrow query ≤ 1.3 × vx_compact and ≤ 0.6 × best Parquet | 1.0–5.3× vx_compact; 1.2–2.3× best Parquet | **fail** on all four queries |
| G5 | map-key filter ≤ 0.5 × best Parquet | 0.33× | pass |
| G6 | swe payload bytes ≤ 0.6 × best Parquet | 27,213 KiB against 31,017 KiB: **0.88×** | **fail** |
| G7 | substring: cold bytes and warm time both ≤ 0.2 × best Parquet | time 0.155× (pass), cold bytes **0.66×** (100% of frames filtered) | **fail** |
| G8 | write ≥ 0.8 × pq_default | 0.20× | **fail** |
| G9 | merge ≤ 0.5 × Parquet | 2.64× | **fail** |

**Outcome: Stop.** Only G2 and G5 pass. The plan's rule is "None of G1a, G6 and G7 pass → Stop", and all three fail on bytes, which no timing can change. Two points were checked before the verdict was written down:

1. **The size gap with the payload prototype is explained, and it does not save G6.** A rerun of `swe` written as 2 files instead of 4 shows where Strata's payload bytes go. A single file is not possible here, because Arrow's 32-bit string offsets overflow at 2.16 GB. Sizes are in KiB:

   | Column | Frames | Refs | Filters | Edges | Strata total | pq_default |
   |---|---|---|---|---|---|---|
   | `input` | 11,762 | 569 | 1,294 | 501 | 14,145 | 21,843 |
   | `output` | 9,356 | 441 | 938 | 507 | 11,255 | 9,657 |
   | both | 21,118 | 1,010 | 2,232 | 1,008 | **25,403 (0.81×)** | 31,500 |

   - **`input` reproduces the prototype.** Its frames alone are 0.54× of Parquet, close to the prototype's 0.62×. Every agent step resends the history, so 2,237,498 chunk references resolve to only 69,730 unique chunks.
   - **`output` has nothing to deduplicate.** Its 112,708 references point to 59,801 unique chunks, because each answer is new text. Its 64 KiB frames come out the same size as Parquet's 1 MiB pages, at 0.97×. Filters and edges then add 20% on top.
   - **The prototype's "38–64% smaller" counted neither filters nor `output`.** Its own table gives 22.1 MiB against 27.8 MiB, or 0.79×, for 64 KiB frames *with* trigram filters. That matches this run's 0.81×. The 0.6× target in G6 was set from the figure without filters.
   - **Dedup scope matters, but only a little.** Going from 4 files to 2 moves the ratio from 0.88× to 0.81×. Even with no filters, edges or refs at all, frames alone would be 0.67×, so G6 fails at any file count.
2. **Where the lookup bytes go (not measured directly).**
   - On `payload`, a warm full lookup reads 1,210 KiB, and a lookup of the light columns reads 141 KiB.
   - So roughly 1 MiB goes to the payload columns and the remaining narrow columns, against the ~21 KiB per payload column that the 0.5 MB target assumed.
   - Splitting that read by column, the way the size breakdown above does, is the next diagnostic if the lookup path is pursued.

**What is worth keeping.** The verdict is about building a whole new format. Some parts of Strata did win, and could be reused on their own:
- **Trigram-filtered search:** 0.15× of Parquet's time warm, and about 0.5× on S3.
- **The no-engine lookup path:** 0.02–0.08× of the time of a Parquet reader without an engine.
- **The shredded map key:** 0.33× of the best Parquet.

The README's cheaper alternative is to store payloads as a sidecar next to Parquet. It would keep the search win without the narrow layer, which is where Strata is slowest (G4, G3), and without the write and merge costs (G8, G9).

## Strata v1 results (M8)

Strata v1 is v0 with the changes below, each made after a profile or a byte count on the M7 datasets. The go/no-go is the plan's, unchanged, measured with the same `fmtbench` on the same VM. Times are local disk, minimum of 30 runs, every format measured in the same run.

### What changed from v0

| Area | Change | Why |
|---|---|---|
| DataFusion scan | Each file is split into row-range partitions, about `target_partitions` in all; files that no chunk can match are dropped first (`StrataFile::may_match`). | v0 gave one partition per file: 4 cores of 12. |
| Scan internals | Chunks are found once per chunk, not once per row; contiguous runs are sliced instead of gathered; no error value is built on the success path. | Per-row binary searches and `BTreeSet` inserts took about 30% of the CPU. |
| Dictionary strings | Decoded as `Dictionary(UInt32, Utf8)` over the file dictionary's shared values; comparisons run on codes (the dictionary is sorted); `Utf8View` is built only for DataFusion's output. | v0 copied every string twice. |
| Decoding | Bit-unpacking reads one unaligned word per value; numeric chunks decode in one pass into the Arrow buffer. | Three intermediate copies per chunk. |
| Dictionaries | Up to 1 MiB (was 64 KiB), so `user_id` gets one. | Equality on codes, and chunk postings. |
| Chunk postings | The key filter segment of a dictionary column also lists the chunks holding each bucket of codes. | `user_filter` read every `user_id` chunk to find a user in a few. |
| Payload store | One chunk store per file for all payload columns, rows interleaved across columns; previews get stores of their own. | An answer and its copy in the next call's input then share frames (E2 in `format-survey.md`). |
| Payload search | Trigram filters and edge groups off by default (`Config::payload_filters`); `Contains` then reads every value. | They cost about 20% of the payload, which G6 cannot afford. |
| Frames | zstd level 9 (was 3). | 10% smaller payloads; writes are slower. |
| Lookup I/O | Point reads merge gaps under 4 KiB (was 64 KiB); refs pages hold 1,024 rows (was 4,096); a file is opened with a 64 KiB suffix read (was 256 KiB). | Gap merging doubled a lookup's bytes; the hot tails measure 12–110 KiB. |

The merge no longer copies payload frames byte for byte: with one store per file, a column's frames are not its own. It re-chunks instead.

### Go / no-go

| # | Criterion | Measured | Verdict |
|---|---|---|---|
| G1a | warm lookup ≤ 0.5 MB, ≤ 2 waves, best Parquet ≥ 2× the bytes | payload: 442 KiB, ≤ 2 waves, best Parquet (pq_cetra) 2,610 KiB = 5.9×; swe: 281 KiB, best Parquet (pq_tuned) 615 KiB = 2.19× | **pass** |
| G1b | cold lookup ≤ 0.5 × best Parquet bytes, ≤ 4 waves | payload: 1,388 KiB against 4,658 = 0.30×; swe: 651 against 2,663 = 0.24×; 4 waves | **pass** |
| G2 | `st_fast` ≤ 0.5 × `pq_fast` | all columns 4.49 against 322.2 ms = 0.014×; light 0.48 against 7.74 ms = 0.06× | pass |
| G3 | `st` total ≤ 0.75×, execute ≤ 0.5× of best Parquet | all columns: total 5.66 against 11.94 ms = 0.47×, execute 4.64 against 9.44 ms = 0.49×; light: total 0.90 against 4.96 = 0.18×, execute 0.57 against 3.25 = 0.18× | pass |
| G4 | each narrow query ≤ 1.3 × vx_compact and ≤ 0.6 × best Parquet | see the table below | **pass** |
| G5 | map-key filter ≤ 0.5 × best Parquet | 3.25 against 33.59 ms = 0.10× | pass |
| G6 | swe payload bytes ≤ 0.6 × best Parquet | 17,586 KiB against 31,017 KiB = **0.57×** | **pass** |
| G7 | substring search | not judged: no frame has a filter. Measured anyway: 472 ms per needle against 491 for pq_cetra and 206 for vx_compact; cold bytes 0.99× | not judged |
| G8 | write ≥ 0.8 × pq_default | 83 MiB/s of Arrow on `payload` (`restrata`), against about 534 for pq_default in M7.4: about 0.16× | fail |
| G9 | merge ≤ 0.5 × Parquet | 49.2 s against 10.3 s = 4.76× | fail |

**Outcome: Go.** G1a, G1b, G4 and G6 pass, and two of the others fail (G8, G9), which the plan's first rule allows.

**Narrow queries on `light` (G4), ms, minimum of 30:**

| Query | `st` | vx_compact | Best Parquet | Limit (min of 1.3 × vx_compact, 0.6 × best Parquet) |
|---|---|---|---|---|
| count_errors_6h | 4.69 | 5.70 | 10.41 (pq_cetra) | 6.25 |
| model_rollup_day | 21.96 | 18.92 | 37.90 (pq_cetra) | 22.74 |
| list_llm_1h | 17.92 | 18.44 | 41.58 (pq_tuned) | 23.97 |
| user_filter | 3.13 | 8.52 | 6.26 (pq_tuned) | 3.76 |

v0 took 12.72, 79.56, 98.51 and 11.45 ms.

### Caveats

- **Thin margins.** `model_rollup_day` passes by 3%, and G3's all-column execute time by 2%. A noisier run could flip either.
- **The "0.5 MB" of G1a** is read as 500,000 bytes (488 KiB); `payload` reads 442 KiB.
- **G1a and G1b were measured on local disk.** They count bytes and waves, which do not depend on the store, but the S3 run of M7.4 was not repeated.
- **G8 compares** a Strata write measured with `restrata` in this run against the Parquet write speed measured in M7.4 on the same machine.
- **Search is gone.** The filters can still be turned on (`payload_filters`), which brings back v0's search speed and puts G6 out of reach. A search index that fits in G6's budget is still open (`format-survey.md` § 2).
- **Writes and merges** are where v1 is weakest; the two-tier design of `format-survey.md` § 4 is the lead.

## Strata v2 results (M9)

Strata v2 rebuilds v1's payload layer to win back search without losing v1's size, and speeds up writes, merges and narrow decoding. The go/no-go is the plan's, unchanged, measured with the same `fmtbench` on the same VM. The design is in [prototype-plan.md](prototype-plan.md) § S4 in v2.

**Runs.** Times are local disk, minimum of 30 runs, every format measured in the same run. Sizes, writes and the merge come from a run on 2026-11-17 (writes: fastest of 5, interleaved with Parquet's; `FMT_REPEAT=5`). Lookups, search and the profile come from a later run that day, after the last reader change. The full payload scan comes from a run on 2026-11-18, after the scan change; lookups, search and narrow queries in that run matched the earlier one. `light` queries come from the first run: nothing it reads changed after it. Other sessions shared the VM; the load average was 1–5 during most runs.

### What changed from v1

| Area | Change | Why |
|---|---|---|
| Chunking | Pieces of 256 B to 4 KiB (was 512 B to 8 KiB), from units of 2 KiB or more. | Near copies of mid-size tool outputs now dedup: −13% of payload bytes on `swe`. |
| Chunk reuse | A value equal to, or extending, the previous value of its column reuses its chunk ids. A value of 256 B or more seen anywhere earlier in the store (by `xxh3_128`) reuses them too. | On `payload`, the bytes cut per file drop from 477 to 325 MiB. |
| Stores | A store holds runs: one per write, several after a merge. The shared store's frames use zstd level 3 with a 64 KiB dictionary trained per store. The preview stores use level 1 and no dictionary. | Level 9 (v1) made writes slow. The dictionary wins back the size. |
| Search | A 5-gram index per run: one bit per frame and row, stored bit-sliced in 1 KiB pages. Every store has one. `LIKE '%x%'` is pushed down exactly, so DataFusion keeps no filter above the scan. | v1 turned the trigram filters off to fit G6. The index costs about 14% of the frames' size. |
| Merge | Stores are copied run by run: frames, index pages and dictionaries byte for byte, chunk ids shifted. Only the refs pages are written again. | v1 re-chunked every payload on merge. |
| Narrow chunks | 2,048 rows (was 4,096); kept raw unless zstd saves at least 30% (was 15%); bit-unpacking writes the Arrow type directly. | A lookup reads one chunk per column; scans skip decompressing chunks that zstd barely shrinks. |
| Point reads | Columns that share a store fetch and decompress each frame once. A refs page is decoded only up to the last row wanted. | A frame was decoded once per column (up to 4 times); a page's rows past the last wanted one were decoded for nothing. |
| Index build | One byte per row while a frame is open, then an 8×8 bit transpose per 8 frames. | Setting bits in place made each gram wait on the previous store. |
| Full scans | Frames are decoded once per file, lazily and on several threads; values are built in 8,192-row pieces, one thread per core, as `Utf8View` blocks of up to 256 KiB; each thread keeps one zstd context per dictionary. | A full payload scan took 674 ms, 2.2× Parquet; it now takes 387 ms. |

### Go / no-go

| # | Criterion | Measured | Verdict |
|---|---|---|---|
| G1a | warm lookup ≤ 0.5 MB, ≤ 2 waves, best Parquet ≥ 2× the bytes | payload: 392 KiB, 2 waves, best Parquet (pq_cetra) 2,610 KiB = 6.7×; swe: 260 KiB, 2 waves, best Parquet (pq_tuned) 615 KiB = 2.4× | **pass** |
| G1b | cold lookup ≤ 0.5 × best Parquet bytes, ≤ 4 waves | payload: 1,160 KiB against 4,658 = 0.25×; swe: 656 against 2,663 = 0.25×; 4 waves | **pass** |
| G2 | `st_fast` ≤ 0.5 × `pq_fast` | all columns 3.86 against 313.0 ms = 0.012×; light 0.35 against 8.26 ms = 0.04× | pass |
| G3 | `st` total ≤ 0.75×, execute ≤ 0.5× of best Parquet | all columns: total 4.97 against 12.89 ms (pq_tuned) = 0.39×, execute 3.99 against 9.66 = 0.41×; light: total 0.73 against 5.48 (pq_cetra) = 0.13×, execute 0.39 against 3.25 = 0.12×. With one partition: all columns 0.43× and 0.44×, light 0.17× and 0.14× | pass |
| G4 | each narrow query ≤ 1.3 × vx_compact and ≤ 0.6 × best Parquet | see the table below | **pass** |
| G5 | map-key filter ≤ 0.5 × best Parquet | 3.59 against 33.95 ms (pq_cetra) = 0.11× | pass |
| G6 | swe payload bytes ≤ 0.6 × best Parquet | 17,415 KiB against 31,019 KiB = **0.56×** | **pass** |
| G7 | substring search ≤ 0.2 × best Parquet, warm time and cold bytes | warm 20.5 against 506.5 ms per needle (pq_default) = 0.04×; cold 3,466 against 21,289 KiB = 0.16×; every frame has an index | pass |
| G8 | write ≥ 0.8 × pq_default | `payload`: 477 against 548 MiB/s of Arrow = 0.87× | pass |
| G9 | merge ≤ 0.5 × Parquet | 2.75 s against 10.16 s = 0.27× | pass |

**Outcome: Go, with every criterion passing.**

**Narrow queries on `light` (G4 and G5), ms, minimum of 30:**

| Query | `st` | vx_compact | vx_default | Best Parquet | Limit (min of 1.3 × vx_compact, 0.6 × best Parquet) |
|---|---|---|---|---|---|
| count_errors_6h | 5.16 | 5.76 | 4.15 | 10.59 (pq_cetra) | 6.35 |
| model_rollup_day | 21.38 | 18.92 | 14.28 | 37.49 (pq_cetra) | 22.49 |
| list_llm_1h | 17.30 | 18.64 | 10.97 | 42.00 (pq_tuned) | 24.23 |
| user_filter | 2.73 | 8.00 | 4.59 | 6.55 (pq_tuned) | 3.93 |
| metadata_filter | 3.59 | errors | errors | 33.95 (pq_cetra) | 16.98 (G5: 0.5 × best Parquet) |

### Against Parquet and Vortex

| | `st` | pq_default | Best Parquet | vx_compact | vx_default |
|---|---|---|---|---|---|
| `payload` size, MiB (4 files) | **260** | 332 | 332 (pq_default) | 316 | 487 |
| `light` size, MiB | 177 | 151 | 151 | **136** | 210 |
| `swe` size, MiB | **25** | 36 | 36 | 31 | 560 |
| warm full-trace lookup, `payload`, ms | **4.83** | 23.80 | 12.22 (pq_tuned) | 32.06 | 16.92 |
| warm full-trace lookup, `payload`, KiB | **392** | 6,350 | 2,610 (pq_cetra) | 72,937 | 71,638 |
| cold full-trace lookup, `payload`, KiB | **1,160** | 8,398 | 4,658 (pq_cetra) | 72,161 | 72,462 |
| substring, `swe`, ms per needle (warm, one partition) | **20.5** | 506.5 | 506.5 (pq_default) | 201.4 | 348.6 |
| substring, `swe`, cold KiB per needle | **3,466** | 21,289 | 21,289 (pq_default) | 18,972 | 548,808 |
| full payload scan, `payload`, ms | 387 | 327 | 305 (pq_cetra) | 252 | **248** |
| write, `payload`, MiB/s of Arrow | 477 | **548** | | | |

Sizes of Parquet and Vortex are from M7.4; `pq_default`'s write speed is from the v2 run.

### Caveats

- **Full payload scans are 1.27× slower than Parquet** (387 against 305 ms; 1.10× on `swe`) although they read 0.6× of its bytes, and use 2.3× its CPU: every frame is decompressed and every value rebuilt from its chunks. A partition also builds a whole file's values before DataFusion consumes any, where Parquet streams and reuses its buffers (peak RSS 4.6 GB against 0.84 GB). A scan that projects no payload column is not affected.
- **`light` files are 17% bigger than pq_default's** (177 against 151 MiB), and 30% bigger than vx_compact's. The narrow layer stores more chunks raw since v2 (30% rule) and in smaller chunks (2,048 rows).
- **Writes of `light` run at 0.59× of Parquet's speed.** G8 is judged on `payload`, as planned; `swe` writes at 1.20×.
- **`model_rollup_day` is slower than vx_compact** (21.4 against 18.9 ms), within the 1.3× allowed; its G4 margin against 0.6 × best Parquet is 5%.
- **The VM was shared.** Other sessions ran on it; the load average reached 5 during some runs. Each figure is a minimum, and Parquet and Vortex ran in the same runs.
- **G1a and G1b were measured on local disk.** They count bytes and waves, which do not depend on the store, but the S3 run of M7.4 was not repeated.

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
