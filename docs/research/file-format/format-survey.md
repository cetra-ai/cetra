# What other formats teach a Strata v1

Strata v0 failed its go/no-go (README § "Strata v0 results (M7.4)"). Before designing a v1, this survey looks at how formats outside the columnar-analytics world (logs, search engines, genomics, video, cloud-hosted images, key-value stores, time series) handle the problems v0 ran into. Parquet and Vortex are deliberately left out.

**Method.** Four Sonnet agents each read primary sources (specs, papers, official docs and design docs) on one topic. Each mapped its findings onto v0's measured failures. I spot-checked the claims this document relies on (see § Verification). Numbers from sources carry a link. Estimates are marked **[est]**.

## v0's failures and their causes

| Failure | Measured | Cause |
|---|---|---|
| G6, payload size | 0.81–0.88× of Parquet; target 0.6× | `output` frames are 0.97× because each answer is new text. Filters and edges add 20%. |
| G7, cold search bytes | 0.66×; target 0.2× | Every frame has a filter, and a cold search reads all of them before it can skip a frame. |
| G1a, lookup bytes | 0.65–0.73× | A trace is spread over one range per column per chunk. |
| S3 cold lookup | 265–338 ms for 4 waves; pq_default takes about 150 ms for 110 requests | Not measured. Likely the serial opens of 4 files (see § 3). |
| G4, narrow queries | 1.2–2.3× slower | Basic light-column encodings. |
| G8, write | 0.20× of Parquet | CDC, hashing, filters and zstd all run at write time. |
| G9, merge | 2.64× slower | Light columns are re-encoded, and filters are rebuilt. |

## The biggest lever: dedup across columns

A finding from reading the v0 code, not from the sources. `write_payload_column` (`strata/src/payload/write.rs`) builds a new `FrameBuilder` for each column, so **dedup works within a column, never across columns**.

In an agent trace, step N's `output` comes back as an assistant message inside step N+1's `input`, and in every later input. Inside `input`, those repeats already collapse: 2,237,498 chunk references resolve to 69,730 unique chunks. But each answer is still stored twice: once in `output`'s frames, which are 9,356 KiB on `swe`, and once in `input`'s.

If most outputs reappear byte for byte inside a later input, sharing one chunk store across the payload columns would remove most of `output`'s frames. Arithmetic on the 2-file `swe` breakdown **[est]**:
- the frames would come to 0.4–0.5× of Parquet, against 0.67× today;
- with posting lists replacing filters and edges (§ 2), G6's 0.6× would come within reach.

Two things decide whether this works, and neither is measured yet:
1. **Byte identity.** Does an output's text appear in the next input with the same bytes? Different JSON escaping or a different envelope around the message would break chunk matches.
2. **Agent design.** How many outputs are never resent? The last step's output never is, and some agents truncate their history.

The payload survey reached the same idea from the other direction (μSlope-style message hashing). Experiment E2 measures it without changing the format.

## 1. Payload compression

**Sources.**
- **CLP** splits log messages into a template, dictionary variables and encoded numbers. Its dictionaries act as an index. ([paper](https://www.usenix.org/system/files/osdi21-rodrigues.pdf))
- **μSlope / clp-s** (OSDI '24) applies the same idea to JSON:
  - a merged tree of the key paths seen;
  - one table per schema, stored by column;
  - single-word strings in a dictionary, and free text parsed with CLP.

  The agent reports an average ratio of 68.1:1, which is 2.34× Zstd's, on 21 log datasets. I could not re-check that figure. ([paper](https://www.usenix.org/system/files/osdi24-wang-rui.pdf))
- **CRAM** stores each data series in its own block, each with its own codec (rANS, a name tokeniser, context models). It compresses against a reference genome, with the reference's MD5 recorded in the file. It recommends 1 MB containers as the unit of random access. ([spec rendering](https://cmdcolin.github.io/hts-specs-md/CRAMv3), [3.1 codecs](https://pmc.ncbi.nlm.nih.gov/articles/PMC8896640))
- **Git packs** choose a delta base by sorting objects on cheap metadata (type, name hash, size), then trying the previous *n* objects (window 10, depth 50 by default). The docs warn that deep chains slow reads. ([pack heuristics](https://git-scm.com/docs/pack-heuristics), [repack](https://git-scm.com/docs/git-repack))
- **zstd dictionaries** help inputs "of just a few hundred bytes". `--patch-from` uses an old file as the dictionary. ([Meta post](https://engineering.fb.com/2018/12/19/core-infra/zstandard/), [v1.4.5 notes](https://github.com/facebook/zstd/releases/tag/v1.4.5))
- **Lucene** compresses stored fields in blocks with shared dictionaries. ([format doc](https://lucene.apache.org/core/10_0_0/core/org/apache/lucene/codecs/lucene90/Lucene90StoredFieldsFormat.html))

**Transfer.**
- **Parse the JSON envelope** (role, tool name, argument keys, ids, finish reason) into typed columns, and keep only the real text as blobs. This removes structure interleaved with prose from the frames. It also makes "find calls to tool X" a dictionary lookup.
- **Hash at message granularity** across columns: the cross-column dedup above, made exact.
- **Dictionaries** are worth it only if the compression unit shrinks to a single message. At 64 KiB frames, expect a small gain **[est]**.
- **Delta encoding** (git, `--patch-from`) adds little to `input` over CDC, and nothing to `output` **[est]**.
- **Context-mixing models** (PPM, CRAM's fqzcomp) compress prose better but decode about 10× slower **[est]**. They would suit a cold tier only.

## 2. Search: posting lists instead of filters per frame

**Sources.**
- **Google Code Search** uses one sorted list of documents per trigram, with no positions. The index "tends to be around 20% of the size of the files being indexed": 420 MB of Linux source gives a 77 MB index. **Verified.** ([Cox](https://swtch.com/~rsc/regexp/regexp4.html))
- **Zoekt** stores positional trigrams and tests the two rarest trigrams at the right distance from each other. The index is about 3× the corpus. ([design](https://github.com/sourcegraph/zoekt/blob/main/doc/design.md))
- **GitHub Blackbird** uses sparse n-grams: variable-length grams chosen by bigram weights, because plain trigrams such as `for` are not selective. ([blog](https://github.blog/engineering/architecture-optimization/the-technology-behind-githubs-new-code-search/))
- **Lucene and Tantivy** encode postings in blocks of 128, bit-packed, with skip data. Merges remap document ids and merge the sorted lists. ([Lucene](https://lucene.apache.org/core/9_10_0/core/org/apache/lucene/codecs/lucene99/Lucene99PostingsFormat.html), [Tantivy](https://github.com/quickwit-oss/tantivy/blob/main/ARCHITECTURE.md))
- **Quickwit** keeps a "hotcache" of about 10 MB per 15 GB split, which opens a split on S3 in under 60 ms. ([architecture](https://quickwit.io/docs/overview/architecture))
- **ClickHouse** docs say the bloom skip indexes (`ngrambf_v1`, `tokenbf_v1`) are deprecated in favour of an inverted text index, which supports n-gram and sparse-gram tokenizers. ([text indexes](https://clickhouse.com/docs/engines/table-engines/mergetree-family/textindexes))
- **Grafana Loki** reported n-gram blooms at about 2% of the log size, then moved its blooms to structured metadata only. No source gives a technical reason. ([2024 post](https://grafana.com/blog/2024/05/29/grafana-loki-query-acceleration-how-we-sped-up-queries-without-adding-resources), [docs](https://grafana.com/docs/loki/latest/operations/query-acceleration-blooms/))

**Transfer.**
- Build **trigram → chunk-id posting lists over unique chunks**. Put boundary trigrams, taken from `tail63 ‖ head63`, in the left chunk's list, which replaces edge groups.
- Keep the directory in the always-read footer. A cold search for a rare needle then reads the directory, the 2–3 rarest lists, then the candidate chunks: about 3 waves **[est]**.
- Index size follows the number of unique chunks, not rows. That should come out well under Cox's 20% of raw text, but it needs measuring (E3).
- A merge adds a constant to chunk ids and concatenates the lists, so no text is reread.
- Variants to measure:
  - drop trigrams found in more than x% of chunks;
  - use sparse n-grams;
  - keep blooms, but store them bit-sliced so a query reads only its own bit rows.

## 3. Layout for lookups over S3

**Sources.**
- **PMTiles v3** puts the header and root directory in the first 16 KiB, with at most one level of leaf directories. Entries are run-length coded, and a "clustered" flag makes offsets implicit. ([spec](https://github.com/protomaps/PMTiles/blob/main/spec/v3/spec.md))
- **COG and GDAL** read all the metadata in one speculative first GET and use HTTP/2 multiplexing. ([GDAL options](https://gdal.org/en/stable/user/configoptions.html))
- **Zarr v3 sharding** writes an index of (offset, nbytes) per chunk at the end of the shard: index then chunk is 2 waves. ([spec](https://zarr-specs.readthedocs.io/en/latest/v3/codecs/sharding-indexed/index.html))
- **Git `.idx` v2** has a 256-entry fanout table, the sorted names, then the offsets. ([format](https://git-scm.com/docs/gitformat-pack))
- **Fragmented MP4 (`moof`+`mdat`, `sidx`) and Matroska (Clusters, Cues, a SeekHead, unknown-size elements)** let a writer stream with the index at the end. ([GPAC](https://wiki.gpac.io/Howtos/dash/DASH-basics/), [RFC 8794](https://www.rfc-editor.org/rfc/rfc8794.html))
- **SQLite overflow pages** are the counter-example: a chained read per page, which is the worst possible pattern on S3. ([format](https://www.sqlite.org/fileformat2.html))

**Transfer.**
- **Store payloads trace-major.** Each trace becomes one contiguous block holding its spans' light values and payload references. A lookup is then one range read.
- **Store light columns column-major as well**, as a scan section. On `light` that is about 5% of the file **[est]**.
- **Put a fanout index in the footer**, for example 2^12 buckets, with entries of an 8-byte id prefix and a length, and offsets implied because blocks are contiguous. The catalog records where the index starts, so wave 1 fetches exactly the index and wave 2 fetches the trace.
- **Choose a dedup scope.** Dedup across traces makes a trace's chunks non-contiguous. Either dedup within the trace block only, or accept one more wave for shared chunks such as system prompts and tool definitions. That last choice is the CRAM reference idea.
- **Possible cause of the S3 anomaly.** Four files opened one after another, each with a HEAD and then a footer read, is about 8 sequential round trips, roughly 200 ms at 25 ms each. That fits the gap. Fix it with a suffix range GET, which returns the size and the tail in one request, with parallel opens, and with footer offsets kept in the catalog. Measure it first (E1).

## 4. Writes and merges

**Sources.**
- **Lucene's stored-fields merge** copies compressed chunks as they are and rewrites only the index, when the codec settings match and the segment has no deletions. Otherwise it falls back to decoding. ([writer source](https://github.com/apache/lucene/blob/main/lucene/core/src/java/org/apache/lucene/codecs/lucene90/compressing/Lucene90CompressingStoredFieldsWriter.java))
- **The zstd seekable format** is independent frames plus a seek table, so concatenation is arithmetic on the tables. ([spec](https://github.com/facebook/zstd/blob/dev/contrib/seekable_format/zstd_seekable_compression_format.md))
- **WiscKey and RocksDB BlobDB** keep large values out of compaction. BlobDB reports write amplification of 1.0–1.02 on the initial load, against 1.6 for leveled compaction. ([BlobDB](https://rocksdb.org/blog/2021/05/26/integrated-blob-db.html), [WiscKey](https://files.usenix.org/s3fs-public/cdn/fast16-papers-lu.pdf))
- **Prometheus** keeps the head block in memory behind a WAL. Compaction merges 2-hour blocks, and deletions are tombstones. ([storage](https://prometheus.io/docs/prometheus/latest/storage/))

**Transfer.**
- **Two tiers.**
  - The ingest tier writes plain blocks: no CDC, no hashing, no filters, and fast zstd.
  - Compaction does the expensive work once: sorting, dedup, the posting index and rollups.
  - G8 then compares the ingest tier with Parquet. The compacted tier is judged on its throughput as a background job.
- **Self-contained blocks** with a codec header per block, holding their own FOR base, dictionary and so on. A merge copies a block when its parameters match the output and its key range doesn't overlap another input; otherwise it decodes the keys and gathers. Deletions are tombstones until a dirty ratio forces a rewrite, as in Lucene.

## 5. Light columns and narrow queries

**Sources.**
- **FastLanes** reports bit-packing, FOR and delta at "as little as 17 CPU cycles" per 1,024 values. ([paper](https://www.vldb.org/pvldb/vol16/p2132-afroozeh.pdf))
- **ALP** encodes floats that originated as decimals as integers per vector of 1,024 values. The agent reports 16.4 bits per value on time series and about 55× Gorilla's decode speed. I could not re-check that figure. ([paper](https://ir.cwi.nl/pub/33334/33334.pdf))
- **Gorilla** reports 1.37 bytes per point, but on metrics sampled at fixed intervals, and it decodes bit by bit. ([paper](https://www.vldb.org/pvldb/vol8/p1816-teller.pdf))
- **FSST** compresses short strings about 2×, with random access to each string. ([paper](https://www.vldb.org/pvldb/vol13/p2649-boncz.pdf))
- **Thanos** keeps mergeable rollups of count, sum, min and max. Its docs say downsampling does not save space and that its purpose is query speed. ([docs](https://thanos.io/tip/components/compact.md/))

**Transfer.** Pick encodings per column:

| Column | Encoding |
|---|---|
| Times sorted within a trace | Delta plus FOR, bit-packed in FastLanes layout |
| Durations and token counts | FOR, with patched exceptions |
| Costs | ALP, with a raw fallback |
| Low-cardinality strings | A dictionary per block |
| Gorilla and Chimp | Not used: span times are not at fixed intervals, and their decode is slow |

Also add a per-file rollup of count, sum, min and max by (time bucket, model, status), merged by addition. It targets `model_rollup_day` and `count_errors_6h`.

## 6. Extensibility

- **PNG** encodes a chunk's properties in the case of its name's letters: a decoder must reject an unknown critical chunk, and may skip an unknown ancillary one. ([PNG 3](https://www.w3.org/TR/png-3/))
- **EBML** separates the version a writer used from the minimum version a reader needs. ([RFC 8794](https://www.rfc-editor.org/rfc/rfc8794.html))

**Transfer.** A section table in the footer with tagged sections, a critical bit per section, and one integer for the minimum reader version. New indexes can then be added without a format bump.

## A v1 sketch

1. **Ingest tier:** WAL, then memtable, then a file of plain blocks with an index at the end.
2. **Compacted file:**
   - trace blocks sorted by trace id, holding the light values and the payload refs;
   - one chunk store shared by all payload columns, holding the parsed message envelope plus text blobs;
   - column-major light sections with FastLanes/ALP blocks of 1,024 values;
   - a trigram posting index;
   - a rollup section;
   - a footer holding the fanout, the section table and the minimum reader version.
3. **Lookups:** 2 waves. **Search:** about 3 waves. **Merge:** copy blocks, remap ids, concatenate lists.

## Experiments, cheapest first

| # | Experiment | Decides |
|---|---|---|
| E1 | Log every S3 request in v0 (path, start, end, bytes) and draw one cold lookup as a Gantt chart. Try a suffix GET and parallel opens. | Whether the S3 anomaly comes from the format or from opening files |
| E2 | Offline count on `swe` and `payload`: unique bytes with one chunk store across `input` and `output` against one per column; the same with message-level hashing; the share of outputs found byte for byte in a later input. | Whether G6 is reachable at all, and therefore whether a v1 is worth it |
| E3 | Build trigram posting lists over the unique chunks. Measure size against filters plus edges (3,240 KiB on `swe`), then cold bytes and waves for the 20 G7 needles. Variants: a df cutoff, sparse grams. | The search index design |
| E4 | v0 write bench with CDC, hashing, filters and zstd disabled one at a time | The ingest tier's budget and which stage costs most |
| E5 | FastLanes-style FOR and delta, and ALP on costs, against the v0 encodings on `light`; re-run `model_rollup_day` and `list_llm_1h` | Whether G4 can close |
| E6 | Frame size, from 16 KiB to 1 MiB, against zstd dictionaries on `output` | The compression unit |
| E7 | A prototype of the trace-major layout with a fanout footer: waves, bytes and cold ms on the S3 rig | G1a |

E1 and E2 are small and can run against existing data. E2 has run: see below.

## E2 result: a shared store helps, but not through dedup

`fmtbench dedup <dir>` reads each `pq_default` file and runs the v0 chunker and frame builder twice: once with one store per payload column, as the writer does, and once with one store shared by all of them (`strata::DedupScope`). The data is the same as M7.4: `swe` written as 2 files, and `payload` as 4.

| Dataset | Frames, one store per column | Frames, one shared store | pq_default, payload columns |
|---|---|---|---|
| `swe` | 21,119 KiB (0.67×) | **17,114 KiB (0.54×)** | 31,501 KiB |
| `payload` | 318,175 KiB (1.20×) | **228,315 KiB (0.86×)** | 264,823 KiB |

**The hypothesis was wrong, but the result is real.**
- Almost no chunk is shared across columns. Only 17 KiB of `output`'s 47,957 KiB of unique chunks on `swe` also appear in `input`, and 1,735 KiB of 480,115 KiB on `payload`.
- The reason is the chunker. An answer under 8 KiB stays one whole JSON element, and that element differs between the two columns. In `output` it ends with `"finish_reason":"stop"`, and in `input` it does not. So the hashes never match.
- The frames still shrink, by 19% on `swe` and 28% on `payload`. In a shared store, an answer and its near-identical copy in the next input are written close together, often in the same 64 KiB frame, where zstd's matching finds the repeat.

**Against G6.**
- The best Parquet is 31,017 KiB on `swe`. The shared frames are 0.55× of that, or 0.58× with v0's refs.
- With v0's filters and edges on top, the total is 0.66×, which fails.
- The 0.6× target therefore depends on a search index costing less than about 600 KiB, against the 3,240 KiB of v0's filters and edges (E3).

**Next lever, not measured.** Cut chunks at message-content boundaries, so an answer's text becomes a chunk of its own and the next input refers to it. Exact dedup would then remove most of `output`'s frames. If none of it were stored twice, the bound would be about `input`'s 11,762 KiB alone, 0.37× **[est]**.

The `payload` dataset is synthetic. With one store per column, its frames are 1.20× of Parquet, larger than Parquet's, so the 64 KiB frames lose to Parquet's 1 MiB pages there. That is not explained.

## Open questions

1. **Are the payload columns' shapes part of the format?** Parsing the message envelope (§ 1) needs to know that a payload is a chat message array. That gives up the "general-purpose columnar file" scope set in README § Scope.
2. **Two tiers or one?** Two tiers fix G8, but mean two readers and two writers.
3. **Should G4 be judged on raw scans, or may rollups answer the dashboard queries?**

## Verification

- **Re-checked against the source:** Cox's 20% figure, and 420 MB → 77 MB.
- **Re-checked against the repo:**
  - the go/no-go targets (`prototype-plan.md`);
  - the 2-file `swe` breakdown (README);
  - dedup per column (`write_payload_column`).
- **Not re-checked:** the μSlope ratios and the ALP figures. The PDFs could not be turned into text on this machine. Treat those numbers as the agents' readings.
- **Agents' own gaps:**
  - not verified from the specs: ZIM, BGZF block size, MP4 faststart, Kafka indexes, Lucene's merge policy;
  - not read: Titan, RFC 3284 (VCDIFF);
  - not found in any source: why Loki dropped n-gram text blooms.
